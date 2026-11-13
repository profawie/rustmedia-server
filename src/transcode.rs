//! Transcoding with ffmpeg for videos the TV can't decode (HEVC/x265,
//! 10-bit H.264, VP9, AV1, ...). Compatible files are served as-is.
//!
//! Incompatible videos are converted in the background, one at a time, to
//! a normal MP4 in a cache folder. Once converted, that file is served with
//! Range support, so seeking works. A video requested before it is ready
//! jumps to the front of the queue and is streamed live meanwhile (no
//! seeking in that case).
//!
//! ffmpeg.exe / ffprobe.exe are looked up next to rustmedia.exe first, then
//! on PATH. Output: H.264 video (max 1080p), every audio track as AAC
//! stereo, and the subtitles (embedded text ones plus sibling
//! .srt/.ass/.vtt files) as selectable mov_text tracks.
//!
//! Why this shape (all checked against VLC 3.0):
//! - subtitles must travel inside the stream: VLC never starts the video
//!   decoder when an external subtitle is attached to an unseekable stream;
//! - VLC's MKV demuxer can't read an unseekable stream, while its MP4
//!   demuxer handles fragmented MP4 fine (used for the live fallback);
//! - fragmented MP4 needs audio parameters up front, which E-AC3 copied from
//!   the source doesn't provide, so audio is re-encoded to AAC.
//!
//! ffmpeg runs in rustmedia's console, so Ctrl+C or closing the window
//! stops it too; an unfinished `.part` file is simply redone next time.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Instant, SystemTime};

pub struct Transcoder {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    encoder: String,
    cache_dir: PathBuf,
    probes: Mutex<HashMap<PathBuf, (SystemTime, Option<Probe>)>>,
    jobs: Mutex<Jobs>,
    wake: Condvar,
    /// Sources whose subtitle-muxed MKV copy is being made right now.
    muxing: Mutex<HashSet<PathBuf>>,
    mux_failed: Mutex<HashSet<PathBuf>>,
}

#[derive(Default)]
struct Jobs {
    queue: VecDeque<Job>,
    /// Output file being made and its progress (0.0-1.0).
    current: Option<(PathBuf, f32)>,
    failed: HashSet<PathBuf>,
}

struct Job {
    src: PathBuf,
    subs: Vec<PathBuf>,
    /// Subtitle file to burn into the picture (Samsung) instead of adding tracks.
    burn: Option<PathBuf>,
    target: PathBuf,
}

/// Where a video stands in the conversion cache.
pub enum CacheState {
    Ready(PathBuf),
    Converting(f32),
    Waiting,
    Failed,
}

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub vcodec: String,
    pub pix_fmt: String,
    pub acodec: String,
    /// Codec of each embedded subtitle stream, in order.
    pub sub_codecs: Vec<String>,
    pub duration: Option<f64>,
}

impl Probe {
    fn video_ok(&self) -> bool {
        // 8-bit 4:2:0 H.264 plays on practically every TV decoder.
        self.vcodec == "h264" && matches!(self.pix_fmt.as_str(), "yuv420p" | "yuvj420p" | "")
    }

    /// Audio is decoded in software by VLC, so only the video codec decides.
    fn needs_transcode(&self) -> bool {
        !self.vcodec.is_empty() && !self.video_ok()
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let p = dir.join(&exe);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(&exe)).find(|p| p.is_file())
}

impl Transcoder {
    /// Find ffmpeg + ffprobe. Returns `None` if either is missing.
    pub fn detect(encoder: String, cache_dir: PathBuf) -> Option<Self> {
        let ffmpeg = find_tool("ffmpeg")?;
        let ffprobe = find_tool("ffprobe")?;
        Some(Transcoder {
            ffmpeg,
            ffprobe,
            encoder,
            cache_dir,
            probes: Mutex::new(HashMap::new()),
            jobs: Mutex::new(Jobs::default()),
            wake: Condvar::new(),
            muxing: Mutex::new(HashSet::new()),
            mux_failed: Mutex::new(HashSet::new()),
        })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn ffmpeg_path(&self) -> &Path {
        &self.ffmpeg
    }

    pub fn encoder(&self) -> &str {
        &self.encoder
    }

    /// Codec info for a file, cached by modification time.
    pub fn probe(&self, path: &Path) -> Option<Probe> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((t, p)) = self.probes.lock().unwrap().get(path) {
            if *t == mtime {
                return p.clone();
            }
        }
        let p = self.run_probe(path);
        self.probes.lock().unwrap().insert(path.to_path_buf(), (mtime, p.clone()));
        p
    }

    fn run_probe(&self, path: &Path) -> Option<Probe> {
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,pix_fmt:format=duration", "-of", "compact"])
            .arg(path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(parse_probe(&String::from_utf8_lossy(&out.stdout)))
    }

    /// True if the file must be transcoded for the TV.
    pub fn needs_transcode(&self, path: &Path) -> bool {
        self.probe(path).is_some_and(|p| p.needs_transcode())
    }

    /// Stream `path` (plus external subtitle files) through ffmpeg to the
    /// client as fragmented MP4.
    pub fn stream(&self, out: &mut TcpStream, path: &Path, subs: &[PathBuf], head: bool) -> io::Result<()> {
        let hdr = format!(
            "HTTP/1.1 200 OK\r\nServer: RustMedia/{}\r\nConnection: close\r\nContent-Type: video/mp4\r\n\
             transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {TC_FEATURES}\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        out.write_all(hdr.as_bytes())?;
        if head {
            return out.flush();
        }
        let mut cmd = self.encode_cmd(path, subs, None);
        cmd.args(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "pipe:1"]);
        let mut child: Child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let res = (|| {
            let mut stdout = child.stdout.take().ok_or_else(|| io::Error::other("no ffmpeg stdout"))?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = stdout.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                out.write_all(&buf[..n])?;
            }
        })();
        // Client gone (or finished): stop ffmpeg right away.
        let _ = child.kill();
        let _ = child.wait();
        res
    }

    /// ffmpeg command with inputs, stream mapping and codecs; the caller adds the output.
    /// `burn`: subtitle file name (relative to ffmpeg's working folder) to
    /// draw into the picture; no subtitle tracks are written then.
    fn encode_cmd(&self, path: &Path, subs: &[PathBuf], burn: Option<&str>) -> Command {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"]).arg(path);
        for sub in subs {
            cmd.arg("-i").arg(sub);
        }
        cmd.args(["-map", "0:v:0", "-map", "0:a?"]);
        // Embedded subtitles: only text ones can become MP4 (mov_text)
        // tracks; picture-based ones (PGS, VobSub) are skipped.
        let embedded: Vec<usize> = if burn.is_some() { Vec::new() } else { self
            .probe(path)
            .map(|p| {
                p.sub_codecs
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| matches!(c.as_str(), "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text"))
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default() };
        for i in &embedded {
            cmd.args(["-map", &format!("0:s:{i}")]);
        }
        for i in 0..subs.len() {
            cmd.args(["-map", &format!("{}:s:0", i + 1)]);
        }
        cmd.args(["-c:v", &self.encoder]);
        if self.encoder == "libx264" {
            cmd.args(["-preset", "veryfast", "-crf", "21", "-tune", "film"]);
        } else {
            cmd.args(["-b:v", "8M"]);
        }
        // Max 1080p, 8-bit 4:2:0: what every TV decoder handles. A keyframe
        // every 2 s keeps fragments small so playback starts quickly.
        // Intel Quick Sync wants NV12; everything else takes yuv420p.
        let pix_fmt = if self.encoder.ends_with("_qsv") { "nv12" } else { "yuv420p" };
        let mut vf = "scale='min(1920,iw)':-2".to_string();
        if let Some(file) = burn {
            vf.push_str(&format!(",subtitles=filename={file}"));
        }
        cmd.args(["-vf", vf.as_str(), "-pix_fmt", pix_fmt, "-profile:v", "high", "-level:v", "4.1"]);
        cmd.args(["-force_key_frames", "expr:gte(t,n_forced*2)"]);
        cmd.args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
        let n_subs = embedded.len() + subs.len();
        if n_subs > 0 {
            // Every track "enabled", so VLC lists and selects them normally.
            cmd.args(["-c:s", "mov_text", "-disposition:s", "default"]);
        }
        // Name external subtitle tracks from their file names (Movie.ms.srt -> Malay).
        for (i, sub) in subs.iter().enumerate() {
            let tag = sub_tag(path, sub);
            let lang = iso639_2(&tag);
            let title = match lang {
                Some((_, name)) => name.to_string(),
                None if tag.is_empty() => "External".to_string(),
                None => tag.clone(),
            };
            let idx = embedded.len() + i;
            cmd.args([format!("-metadata:s:s:{idx}"), format!("title={title}")]);
            cmd.args([format!("-metadata:s:s:{idx}"), format!("handler_name={title}")]);
            if let Some((code, _)) = lang {
                cmd.args([format!("-metadata:s:s:{idx}"), format!("language={code}")]);
            }
        }
        cmd
    }

    // ------------------------------------------------------------ cache

    /// Cache file for a source + its subtitles. The name changes whenever
    /// any of them is modified, so stale conversions are never served.
    fn cache_file(&self, src: &Path, subs: &[PathBuf]) -> PathBuf {
        self.cache_name(src, subs, &self.encoder, "mp4")
    }

    fn cache_name(&self, src: &Path, subs: &[PathBuf], kind: &str, ext: &str) -> PathBuf {
        let mut h = DefaultHasher::new();
        for p in std::iter::once(src).chain(subs.iter().map(PathBuf::as_path)) {
            p.hash(&mut h);
            if let Ok(m) = std::fs::metadata(p) {
                m.len().hash(&mut h);
                m.modified().ok().hash(&mut h);
            }
        }
        kind.hash(&mut h);
        let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.cache_dir.join(format!("{stem}.{:016x}.{ext}", h.finish()))
    }

    pub fn state(&self, src: &Path, subs: &[PathBuf]) -> CacheState {
        self.target_state(&self.cache_file(src, subs))
    }

    fn target_state(&self, file: &Path) -> CacheState {
        if file.is_file() {
            return CacheState::Ready(file.to_path_buf());
        }
        let jobs = self.jobs.lock().unwrap();
        match &jobs.current {
            Some((p, pct)) if p == file => CacheState::Converting(*pct),
            _ if jobs.failed.contains(file) => CacheState::Failed,
            _ => CacheState::Waiting,
        }
    }

    /// Queue a conversion. `urgent` puts it first (someone wants to watch it).
    pub fn enqueue(&self, src: PathBuf, subs: Vec<PathBuf>, urgent: bool) {
        let target = self.cache_file(&src, &subs);
        self.push_job(Job { src, subs, burn: None, target }, urgent);
    }

    /// Copy of `src` with `sub` burned into the picture, for players that
    /// show no subtitle tracks we add (Samsung).
    fn burn_file(&self, src: &Path, sub: &Path) -> PathBuf {
        self.cache_name(src, &[sub.to_path_buf()], &format!("burn1-{}", self.encoder), "mp4")
    }

    /// State of the burned-in copy; queues it (first in line) if missing.
    pub fn burned(&self, src: &Path, sub: &Path) -> CacheState {
        let target = self.burn_file(src, sub);
        let st = self.target_state(&target);
        if matches!(st, CacheState::Waiting) {
            let job = Job { src: src.to_path_buf(), subs: Vec::new(), burn: Some(sub.to_path_buf()), target };
            self.push_job(job, true);
        }
        st
    }

    fn push_job(&self, job: Job, urgent: bool) {
        if job.target.is_file() {
            return;
        }
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.failed.contains(&job.target) || jobs.current.as_ref().is_some_and(|(p, _)| *p == job.target) {
            return;
        }
        let existing = jobs.queue.iter().position(|j| j.target == job.target);
        match (existing, urgent) {
            (Some(_), false) => return,
            (Some(0), true) => return,
            (Some(i), true) => {
                jobs.queue.remove(i);
            }
            _ => {}
        }
        if urgent { jobs.queue.push_front(job) } else { jobs.queue.push_back(job) }
        self.wake.notify_all();
    }

    /// Background thread converting queued videos one at a time.
    pub fn start_worker(&'static self) {
        std::thread::spawn(move || loop {
            let job = {
                let mut jobs = self.jobs.lock().unwrap();
                loop {
                    if let Some(j) = jobs.queue.pop_front() {
                        jobs.current = Some((j.target.clone(), 0.0));
                        break j;
                    }
                    jobs = self.wake.wait(jobs).unwrap();
                }
            };
            let ok = self.convert(&job);
            let mut jobs = self.jobs.lock().unwrap();
            jobs.current = None;
            if !ok {
                jobs.failed.insert(job.target);
            }
        });
    }

    fn convert(&self, job: &Job) -> bool {
        let name = job.src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let target = job.target.clone();
        if target.is_file() {
            return true;
        }
        let part = target.with_extension("mp4.part");
        let duration = self.probe(&job.src).and_then(|p| p.duration).unwrap_or(0.0);
        // The subtitles filter takes a file name inside a filter string, where
        // Windows paths (C:\...) need awkward escaping. Copy the file into the
        // cache folder under a plain name and run ffmpeg there instead.
        let burn_copy = job.burn.as_ref().map(|sub| {
            let ext = sub.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "srt".into());
            let stem = target.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let hash = stem.rsplit('.').next().unwrap_or("sub").to_string();
            (sub.clone(), format!("burn-{hash}.{ext}"))
        });
        if let Some((sub, file)) = &burn_copy {
            if let Err(e) = std::fs::copy(sub, self.cache_dir.join(file)) {
                println!(" Cannot read subtitle {}: {e}", sub.display());
                return false;
            }
            println!(" Burning subtitles into {name} for Samsung (re-encodes the video, takes a while)");
        } else {
            println!(" Converting for TV: {name}");
        }
        let started = Instant::now();

        let mut cmd = self.encode_cmd(&job.src, &job.subs, burn_copy.as_ref().map(|(_, f)| f.as_str()));
        if burn_copy.is_some() {
            cmd.current_dir(&self.cache_dir);
        }
        cmd.args(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats", "-y", "-f", "mp4"]).arg(&part);
        // Below-normal priority, so a video being watched (streamed live)
        // gets the CPU first.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            cmd.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        }
        let mut child = match cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                println!(" Conversion failed to start for {name}: {e}");
                return false;
            }
        };
        // Collect ffmpeg's error output on a side thread so it can't block.
        let stderr = child.stderr.take();
        let errors = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut s);
            }
            s
        });
        let mut last_step = 0;
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) else {
                    continue;
                };
                if duration <= 0.0 {
                    continue;
                }
                let pct = (us / 1e6 / duration).clamp(0.0, 1.0) as f32;
                if let Some((_, p)) = self.jobs.lock().unwrap().current.as_mut() {
                    *p = pct;
                }
                let step = (pct * 10.0) as u32;
                if step > last_step && step < 10 {
                    last_step = step;
                    println!(" Converting {name}: {}%", step * 10);
                }
            }
        }
        let status = child.wait();
        let errors = errors.join().unwrap_or_default();
        if let Some((_, file)) = &burn_copy {
            let _ = std::fs::remove_file(self.cache_dir.join(file));
        }
        if status.is_ok_and(|s| s.success()) && std::fs::rename(&part, &target).is_ok() {
            let mins = started.elapsed().as_secs_f64() / 60.0;
            println!(" Ready for TV (seekable): {name} ({mins:.1} min)");
            true
        } else {
            let _ = std::fs::remove_file(&part);
            let reason = errors.lines().last().unwrap_or("ffmpeg stopped").trim().to_string();
            println!(" Conversion failed for {name}: {reason}");
            false
        }
    }

    // ------------------------------------------------------------ subtitle mux

    /// Copy of `src` with the external subtitle files added as MKV tracks.
    /// Samsung TVs only list subtitles inside the file, never a sibling .srt.
    fn mux_file(&self, src: &Path, subs: &[PathBuf]) -> PathBuf {
        self.cache_name(src, subs, "mux2", "mkv")
    }

    /// The muxed copy, if it has been made.
    pub fn muxed(&self, src: &Path, subs: &[PathBuf]) -> Option<PathBuf> {
        let f = self.mux_file(src, subs);
        f.is_file().then_some(f)
    }

    /// True while the muxed copy of `src` is being made.
    pub fn muxing(&self, src: &Path) -> bool {
        self.muxing.lock().unwrap().contains(src)
    }

    /// Make the muxed copy in the background (streams are copied, not
    /// re-encoded, so it takes about as long as copying the file).
    pub fn start_mux(&'static self, src: PathBuf, subs: Vec<PathBuf>) {
        if subs.is_empty() || self.muxed(&src, &subs).is_some() || self.mux_failed.lock().unwrap().contains(&src) {
            return;
        }
        if !self.muxing.lock().unwrap().insert(src.clone()) {
            return;
        }
        std::thread::spawn(move || {
            if !self.mux(&src, &subs) {
                self.mux_failed.lock().unwrap().insert(src.clone());
            }
            self.muxing.lock().unwrap().remove(&src);
        });
    }

    fn mux_cmd(&self, src: &Path, subs: &[PathBuf], out: &Path) -> Command {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"]).arg(src);
        for sub in subs {
            cmd.arg("-i").arg(sub);
        }
        cmd.args(["-map", "0:v:0", "-map", "0:a?"]);
        // External subtitles first (the preferred language leads), then the
        // embedded ones, then attachments such as fonts for ASS subtitles.
        for i in 0..subs.len() {
            cmd.args(["-map", &format!("{}:s:0", i + 1)]);
        }
        cmd.args(["-map", "0:s?", "-map", "0:t?", "-c", "copy"]);
        // Samsung players show SRT tracks in the list but draw nothing for
        // them; ASS tracks work, so the added files are converted to ASS.
        for i in 0..subs.len() {
            cmd.args([format!("-c:s:{i}"), "ass".to_string()]);
        }
        cmd.args(["-disposition:s", "0", "-disposition:s:0", "default"]);
        for (i, sub) in subs.iter().enumerate() {
            let tag = sub_tag(src, sub);
            let (code, title) = match iso639_2(&tag) {
                Some((code, name)) => (Some(code), name.to_string()),
                None if tag.is_empty() => (None, "External".to_string()),
                None => (None, tag.clone()),
            };
            cmd.args([format!("-metadata:s:s:{i}"), format!("title={title}")]);
            if let Some(code) = code {
                cmd.args([format!("-metadata:s:s:{i}"), format!("language={code}")]);
            }
        }
        cmd.args(["-y", "-f", "matroska"]).arg(out);
        cmd
    }

    fn mux(&self, src: &Path, subs: &[PathBuf]) -> bool {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let target = self.mux_file(src, subs);
        let part = target.with_extension("mkv.part");
        println!(" Adding subtitles for Samsung: {name}");
        let started = Instant::now();
        let out = self.mux_cmd(src, subs, &part).stdin(Stdio::null()).stdout(Stdio::null()).output();
        match out {
            Ok(o) if o.status.success() && std::fs::rename(&part, &target).is_ok() => {
                println!(" Subtitles added: {name} ({:.0} s)", started.elapsed().as_secs_f64());
                true
            }
            other => {
                let _ = std::fs::remove_file(&part);
                let reason = match other {
                    Ok(o) => String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("ffmpeg stopped").trim().to_string(),
                    Err(e) => e.to_string(),
                };
                println!(" Could not add subtitles to {name}: {reason}");
                false
            }
        }
    }
}

pub const TC_FEATURES: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// The part of a subtitle name between the video's stem and the extension:
/// `Movie.mkv` + `Movie.ms.srt` -> "ms".
fn sub_tag(video: &Path, sub: &Path) -> String {
    let stem = video.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let sub_stem = sub.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if sub_stem.len() > stem.len() && sub_stem.to_lowercase().starts_with(&stem) {
        sub_stem[stem.len()..].trim_start_matches('.').to_string()
    } else {
        String::new()
    }
}

fn iso639_2(tag: &str) -> Option<(&'static str, &'static str)> {
    Some(match tag.to_ascii_lowercase().as_str() {
        "ms" | "may" | "msa" | "malay" | "melayu" | "bm" => ("may", "Malay"),
        "en" | "eng" | "english" => ("eng", "English"),
        "id" | "ind" | "indonesian" => ("ind", "Indonesian"),
        "zh" | "chi" | "zho" | "chs" | "cht" | "chinese" => ("chi", "Chinese"),
        "ja" | "jpn" | "japanese" => ("jpn", "Japanese"),
        "ko" | "kor" | "korean" => ("kor", "Korean"),
        "ar" | "ara" | "arabic" => ("ara", "Arabic"),
        "th" | "tha" | "thai" => ("tha", "Thai"),
        "ta" | "tam" | "tamil" => ("tam", "Tamil"),
        _ => return None,
    })
}

fn parse_probe(s: &str) -> Probe {
    let mut p = Probe::default();
    for line in s.lines() {
        let fields: HashMap<&str, &str> = line.split('|').filter_map(|f| f.split_once('=')).collect();
        if line.starts_with("stream") {
            match fields.get("codec_type").copied() {
                Some("video") if p.vcodec.is_empty() => {
                    let codec = fields.get("codec_name").copied().unwrap_or("");
                    // Cover art in MKV/MP4 shows up as an mjpeg/png "video" stream.
                    if !matches!(codec, "mjpeg" | "png" | "bmp") {
                        p.vcodec = codec.to_string();
                        p.pix_fmt = fields.get("pix_fmt").copied().unwrap_or("").to_string();
                    }
                }
                Some("audio") if p.acodec.is_empty() => {
                    p.acodec = fields.get("codec_name").copied().unwrap_or("").to_string();
                }
                Some("subtitle") => p.sub_codecs.push(fields.get("codec_name").copied().unwrap_or("").to_string()),
                _ => {}
            }
        } else if line.starts_with("format") {
            p.duration = fields.get("duration").and_then(|d| d.parse().ok());
        }
    }
    p
}

/// `H:MM:SS.mmm` for DIDL `duration` attributes.
pub fn fmt_duration(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_and_decide() {
        let p = parse_probe(
            "stream|codec_name=hevc|codec_type=video|pix_fmt=yuv420p10le\n\
             stream|codec_name=eac3|codec_type=audio\n\
             stream|codec_name=subrip|codec_type=subtitle\n\
             format|duration=6123.456000\n",
        );
        assert_eq!((p.vcodec.as_str(), p.acodec.as_str()), ("hevc", "eac3"));
        assert_eq!(p.sub_codecs, vec!["subrip".to_string()]);
        assert_eq!(p.duration, Some(6123.456));
        assert!(p.needs_transcode());

        let ok = parse_probe("stream|codec_name=h264|codec_type=video|pix_fmt=yuv420p\nstream|codec_name=flac|codec_type=audio\n");
        assert!(!ok.needs_transcode());
        let ten_bit = parse_probe("stream|codec_name=h264|codec_type=video|pix_fmt=yuv420p10le\n");
        assert!(ten_bit.needs_transcode());
        let art = parse_probe("stream|codec_name=mjpeg|codec_type=video|pix_fmt=yuvj420p\nstream|codec_name=mp3|codec_type=audio\n");
        assert!(!art.needs_transcode());
    }

    #[test]
    fn sub_tags() {
        let v = Path::new("Movie Name.mkv");
        assert_eq!(sub_tag(v, Path::new("Movie Name.ms.srt")), "ms");
        assert_eq!(sub_tag(v, Path::new("Movie Name.srt")), "");
        assert_eq!(iso639_2("ms"), Some(("may", "Malay")));
        assert_eq!(iso639_2("forced"), None);
    }

    #[test]
    fn duration_fmt() {
        assert_eq!(fmt_duration(6123.456), "1:42:03.456");
    }
}
