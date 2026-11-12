//! On-the-fly transcoding with ffmpeg for videos the TV can't decode
//! (HEVC/x265, 10-bit H.264, VP9, AV1, ...). Compatible files are still
//! served as-is with seeking; only incompatible ones go through ffmpeg.
//!
//! ffmpeg.exe / ffprobe.exe are looked up next to rustmedia.exe first, then
//! on PATH. Output is fragmented MP4: video re-encoded to H.264, every
//! audio track as AAC stereo, and the subtitles (embedded text ones plus
//! sibling .srt/.ass/.vtt files) muxed in as selectable tracks.
//!
//! Why this shape (all checked against VLC 3.0):
//! - subtitles must travel inside the stream: VLC never starts the video
//!   decoder when an external subtitle is attached to an unseekable stream;
//! - VLC's MKV demuxer can't read an unseekable stream, while its MP4
//!   demuxer handles fragmented MP4 fine;
//! - fragmented MP4 needs audio parameters up front, which E-AC3 copied from
//!   the source doesn't provide, so audio is re-encoded to AAC.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::SystemTime;

pub struct Transcoder {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    encoder: String,
    cache: Mutex<HashMap<PathBuf, (SystemTime, Option<Probe>)>>,
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

/// `CREATE_NO_WINDOW`, so ffmpeg doesn't flash a console window on Windows.
fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
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
    pub fn detect(encoder: String) -> Option<Self> {
        let ffmpeg = find_tool("ffmpeg")?;
        let ffprobe = find_tool("ffprobe")?;
        Some(Transcoder { ffmpeg, ffprobe, encoder, cache: Mutex::new(HashMap::new()) })
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
        if let Some((t, p)) = self.cache.lock().unwrap().get(path) {
            if *t == mtime {
                return p.clone();
            }
        }
        let p = self.run_probe(path);
        self.cache.lock().unwrap().insert(path.to_path_buf(), (mtime, p.clone()));
        p
    }

    fn run_probe(&self, path: &Path) -> Option<Probe> {
        let out = quiet(
            Command::new(&self.ffprobe)
                .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,pix_fmt:format=duration", "-of", "compact"])
                .arg(path)
                .stdin(Stdio::null())
                .stderr(Stdio::null()),
        )
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
        let mut child = self.spawn(path, subs)?;
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

    fn spawn(&self, path: &Path, subs: &[PathBuf]) -> io::Result<Child> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"]).arg(path);
        for sub in subs {
            cmd.arg("-i").arg(sub);
        }
        cmd.args(["-map", "0:v:0", "-map", "0:a?"]);
        // Embedded subtitles: only text ones can become MP4 (mov_text)
        // tracks; picture-based ones (PGS, VobSub) are skipped.
        let embedded: Vec<usize> = self
            .probe(path)
            .map(|p| {
                p.sub_codecs
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| matches!(c.as_str(), "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text"))
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default();
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
        cmd.args(["-vf", "scale='min(1920,iw)':-2", "-pix_fmt", pix_fmt, "-profile:v", "high", "-level:v", "4.1"]);
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
        cmd.args(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "pipe:1"]);
        quiet(cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())).spawn()
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
