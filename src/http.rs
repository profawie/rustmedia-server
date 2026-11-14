//! Minimal threaded HTTP/1.1 server: directory listing, M3U playlists,
//! file streaming with Range (seek) support, plus the DLNA endpoints.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dlna;
use crate::transcode::{CacheState, Transcoder};
use crate::util::*;

pub struct Ctx {
    pub root: PathBuf,
    pub dlna: Option<&'static dlna::Device>,
    pub tc: Option<&'static Transcoder>,
    /// Preferred subtitle language tag (e.g. "ms"), listed first.
    pub sub_lang: Option<String>,
    /// What kind of player each client address turned out to be.
    pub clients: Mutex<HashMap<IpAddr, Profile>>,
}

/// What a client gets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    /// Samsung TV / Smart Monitor: original files, or with `--sub-lang` an
    /// H.264 copy with those subtitles burned in.
    Samsung,
    /// VLC on a TV (Google TV / Android TV): videos its decoder can't handle
    /// are converted to H.264, subtitles as selectable tracks.
    Tv,
    /// VLC on a PC or Mac: always the original files, full quality.
    Pc,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Profile::Samsung => "Samsung TV (subtitles burned in with --sub-lang)",
            Profile::Tv => "TV with VLC (H.264 copies, subtitle tracks)",
            Profile::Pc => "PC/Mac VLC (original files, full quality)",
        }
    }
}

impl Ctx {
    /// Work out what kind of player sent `req`, and remember it for the
    /// client's address: VLC's own HTTP requests don't say which OS it runs
    /// on, but its UPnP requests do (Windows/Darwin vs Linux/Android).
    pub fn profile(&self, req: &Request, peer: IpAddr, local: IpAddr) -> Profile {
        if let Some(p) = req.query_param("for").and_then(|v| match v.as_str() {
            "pc" | "mac" => Some(Profile::Pc),
            "tv" => Some(Profile::Tv),
            "samsung" => Some(Profile::Samsung),
            _ => None,
        }) {
            return p;
        }
        let ua = req.header("user-agent").unwrap_or("").to_ascii_lowercase();
        let detected = if is_samsung(req) {
            Some(Profile::Samsung)
        } else if ["windows", "darwin", "macintosh", "mac os"].iter().any(|k| ua.contains(k)) {
            Some(Profile::Pc)
        } else if ["android", "linux"].iter().any(|k| ua.contains(k)) {
            Some(Profile::Tv)
        } else {
            None
        };
        let mut clients = self.clients.lock().unwrap();
        let known = clients.get(&peer).copied();
        // Samsung is the strongest signal; never downgrade from it.
        let profile = match (detected, known) {
            (_, Some(Profile::Samsung)) => Profile::Samsung,
            (Some(d), _) => d,
            (None, Some(k)) => k,
            // Same machine as RustMedia: VLC on this PC.
            (None, None) if peer.is_loopback() || peer == local => Profile::Pc,
            (None, None) => Profile::Tv,
        };
        if known != Some(profile) && (detected.is_some() || known.is_none()) {
            clients.insert(peer, profile);
            println!(" Client {peer}: {}", profile.label());
        }
        profile
    }

    /// URL path a client should use to play `rel`: the original file, or
    /// `/_tc/...` when it has to be transcoded for the TV.
    pub fn play_path(&self, rel: &str, profile: Profile) -> (String, bool) {
        let needs_tc = profile == Profile::Tv && self.tc.is_some_and(|tc| {
            mime_for(rel).1 == Kind::Video
                && safe_join(&self.root, rel).is_some_and(|(full, _)| tc.needs_transcode(&full))
        });
        let p = url_encode_path(rel);
        if needs_tc { (format!("/_tc/{p}"), true) } else { (format!("/{p}"), false) }
    }

    /// Sibling subtitle files of the video at `rel` (`Movie.srt`, `Movie.ms.srt`, ...).
    pub fn sibling_subs(&self, rel: &str) -> Vec<PathBuf> {
        let rel = rel.trim_matches('/');
        let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let Some((dir, _)) = safe_join(&self.root, parent) else { return Vec::new() };
        let name = rel.rsplit('/').next().unwrap_or(rel);
        read_entries(&dir, parent)
            .into_iter()
            .find(|e| e.rel == rel)
            .map(|e| {
                order_subs(&e.subs, name, self.sub_lang.as_deref())
                    .iter()
                    .filter_map(|s| safe_join(&self.root, s).map(|(p, _)| p))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// For Samsung with `--sub-lang`: state of the copy of `rel` with that
    /// language's subtitle file burned into the picture (queued if missing).
    /// `None` when there is no such subtitle file.
    pub fn samsung_burn(&self, rel: &str) -> Option<CacheState> {
        let tc = self.tc?;
        let lang = self.sub_lang.as_deref()?;
        if mime_for(rel).1 != Kind::Video {
            return None;
        }
        let (full, _) = safe_join(&self.root, rel)?;
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let sub = self
            .sibling_subs(rel)
            .into_iter()
            .find(|s| sub_tag(&s.to_string_lossy(), name).eq_ignore_ascii_case(lang))?;
        Some(tc.burned(&full, &sub))
    }

    /// For Samsung: the copy of `rel` with its subtitle files muxed in, if
    /// made. Otherwise starts making it and returns `None`.
    pub fn samsung_copy(&self, rel: &str) -> Option<PathBuf> {
        let tc = self.tc?;
        if mime_for(rel).1 != Kind::Video {
            return None;
        }
        let (full, _) = safe_join(&self.root, rel)?;
        let subs = self.sibling_subs(rel);
        if subs.is_empty() {
            return None;
        }
        let found = tc.muxed(&full, &subs);
        if found.is_none() {
            tc.start_mux(full, subs);
        }
        found
    }

    /// Conversion state of a video that needs transcoding (`None` otherwise).
    pub fn tc_state(&self, rel: &str) -> Option<CacheState> {
        let tc = self.tc?;
        let (full, _) = safe_join(&self.root, rel)?;
        if mime_for(rel).1 != Kind::Video || !tc.needs_transcode(&full) {
            return None;
        }
        Some(tc.state(&full, &self.sibling_subs(rel)))
    }
}

/// Put subtitles whose tag (`Movie.<tag>.srt`) matches `lang` first.
/// Clients that show only one subtitle (Samsung) use the first.
pub fn order_subs(subs: &[String], video_name: &str, lang: Option<&str>) -> Vec<String> {
    let mut v = subs.to_vec();
    if let Some(lang) = lang {
        v.sort_by_key(|s| !sub_tag(s, video_name).eq_ignore_ascii_case(lang));
    }
    v
}

/// `Movie.mkv` + `dir/Movie.ms.srt` -> "ms".
fn sub_tag(sub: &str, video_name: &str) -> String {
    let stem = video_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(video_name).to_lowercase();
    let file = sub.rsplit(['/', '\\']).next().unwrap_or(sub).to_lowercase();
    let sub_stem = file.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(file.clone());
    sub_stem.strip_prefix(&stem).unwrap_or("").trim_start_matches('.').to_string()
}

/// Samsung TVs/monitors (Tizen) play HEVC, MKV and external subtitles
/// themselves, so they get the original files, never transcoded ones.
pub fn is_samsung(req: &Request) -> bool {
    let ua = req.header("user-agent").unwrap_or("").to_ascii_lowercase();
    ua.contains("sec_hhp") || ua.contains("samsung") || ua.contains("tizen")
        || req.header("getcaptioninfo.sec").is_some()
}

/// Short note shown next to a transcoded video's name.
pub fn tc_note(state: &CacheState) -> String {
    match state {
        CacheState::Ready(_) => "TV".into(),
        CacheState::Converting(p) => format!("TV, converting {:.0}%, no seeking yet", p * 100.0),
        CacheState::Waiting => "TV, waiting to convert, no seeking yet".into(),
        CacheState::Failed => "TV, conversion failed, no seeking".into(),
    }
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then(|| percent_decode(v, true))
        })
    }
}

pub fn run(ctx: Arc<Ctx>, listener: TcpListener) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle(&ctx, stream) {
                // Clients (VLC especially) drop connections all the time when
                // seeking; only report unusual errors.
                if !matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) {
                    eprintln!("[{}] error: {e}", peer.map(|p| p.to_string()).unwrap_or_default());
                }
            }
        });
    }
}

fn read_request(stream: &TcpStream) -> io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut headers = Vec::new();
    for _ in 0..100 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut req = Request { method, path, query, headers, body: Vec::new() };
    let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > 0 {
        let mut body = vec![0; len.min(1 << 20)];
        reader.read_exact(&mut body)?;
        req.body = body;
    }
    Ok(Some(req))
}

/// Write a complete response with an in-memory body.
pub fn respond(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    head_only: bool,
) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status}\r\nServer: RustMedia/{}\r\nConnection: close\r\nContent-Length: {}\r\n", env!("CARGO_PKG_VERSION"), body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn handle(ctx: &Ctx, mut stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let Some(req) = read_request(&stream)? else { return Ok(()) };
    let local = stream.local_addr()?;
    let peer = stream.peer_addr()?.ip();
    let head = req.method == "HEAD";
    let profile = ctx.profile(&req, peer, local.ip());

    if req.path.starts_with("/_dlna/") {
        return match &ctx.dlna {
            Some(dev) => dlna::handle_http(ctx, dev, &req, &mut stream, local, profile),
            None => respond(&mut stream, "404 Not Found", &[], b"DLNA disabled", head),
        };
    }

    if req.method != "GET" && !head {
        return respond(&mut stream, "405 Method Not Allowed", &[("Allow", "GET, HEAD")], b"", false);
    }

    // A Samsung asking for any version of a video (it may replay an address
    // it remembered from before the burned-in copy existed, e.g. after
    // switching input and back) gets the burned-in copy once it exists.
    if profile == Profile::Samsung && ctx.sub_lang.is_some() {
        let path = req.path.as_str();
        let rest = ["/_burn/", "/_mux/", "/_tc/"].iter().find_map(|p| path.strip_prefix(p)).unwrap_or(path.trim_start_matches('/'));
        let rel = percent_decode(rest, false);
        if mime_for(&rel).1 == Kind::Video {
            if let Some(CacheState::Ready(file)) = ctx.samsung_burn(&rel) {
                let len = fs::metadata(&file)?.len();
                return serve_file(&mut stream, &req, &file, len, head, &[]);
            }
        }
    }

    if let Some(rest) = req.path.strip_prefix("/_tc/") {
        let Some(tc) = ctx.tc else {
            return respond(&mut stream, "404 Not Found", &[], b"transcoding disabled", head);
        };
        let Some((full, _)) = safe_join(&ctx.root, &percent_decode(rest, false)) else {
            return respond(&mut stream, "400 Bad Request", &[], b"bad path", head);
        };
        if !full.is_file() {
            return respond(&mut stream, "404 Not Found", &[], b"Not found", head);
        }
        // Sibling subtitle files are muxed into the output as tracks.
        let subs = ctx.sibling_subs(&percent_decode(rest, false));
        if let CacheState::Ready(file) = tc.state(&full, &subs) {
            // Converted copy: a normal file, so Range requests (seeking) work.
            let len = fs::metadata(&file)?.len();
            return serve_file(&mut stream, &req, &file, len, head, &[]);
        }
        // Not converted yet: move it to the front of the queue and stream
        // it live in the meantime.
        if !head {
            tc.enqueue(full.clone(), subs.clone(), true);
        }
        stream.set_read_timeout(None)?;
        return tc.stream(&mut stream, &full, &subs, head);
    }

    if let Some(rest) = req.path.strip_prefix("/_burn/") {
        // Samsung: the video with the --sub-lang subtitles burned in.
        let rel = percent_decode(rest, false);
        let Some((full, rel)) = safe_join(&ctx.root, &rel) else {
            return respond(&mut stream, "400 Bad Request", &[], b"bad path", head);
        };
        let file = match ctx.samsung_burn(&rel) {
            Some(CacheState::Ready(f)) => f,
            _ => full,
        };
        let Ok(meta) = fs::metadata(&file) else {
            return respond(&mut stream, "404 Not Found", &[], b"Not found", head);
        };
        return serve_file(&mut stream, &req, &file, meta.len(), head, &[]);
    }

    if let Some(rest) = req.path.strip_prefix("/_mux/") {
        // Samsung: the video with its subtitle files inside (see samsung_copy).
        let rel = percent_decode(rest, false);
        let Some((full, rel)) = safe_join(&ctx.root, &rel) else {
            return respond(&mut stream, "400 Bad Request", &[], b"bad path", head);
        };
        let file = ctx.samsung_copy(&rel).unwrap_or(full);
        let Ok(meta) = fs::metadata(&file) else {
            return respond(&mut stream, "404 Not Found", &[], b"Not found", head);
        };
        let extra = samsung_headers(ctx, &req, &rel, local);
        return serve_file(&mut stream, &req, &file, meta.len(), head, &extra);
    }

    if req.path == "/playlist.m3u" || req.path == "/playlist.m3u8" {
        let dir = req.query_param("dir").unwrap_or_default();
        let Some((full, rel)) = safe_join(&ctx.root, &dir) else {
            return respond(&mut stream, "400 Bad Request", &[], b"bad path", head);
        };
        let base = base_url(&req, local);
        let body = playlist(ctx, &full, &rel, &base, profile);
        return respond(
            &mut stream,
            "200 OK",
            &[("Content-Type", "audio/x-mpegurl; charset=utf-8")],
            body.as_bytes(),
            head,
        );
    }

    let decoded = percent_decode(&req.path, false);
    let Some((full, rel)) = safe_join(&ctx.root, &decoded) else {
        return respond(&mut stream, "400 Bad Request", &[], b"bad path", head);
    };
    let meta = match fs::metadata(&full) {
        Ok(m) => m,
        Err(_) => return respond(&mut stream, "404 Not Found", &[("Content-Type", "text/plain")], b"Not found", head),
    };
    if meta.is_dir() {
        if !req.path.ends_with('/') {
            let loc = format!("{}/", req.path);
            return respond(&mut stream, "301 Moved Permanently", &[("Location", &loc)], b"", head);
        }
        // The page is mostly for copying a link into a TV's VLC.
        let html = listing(ctx, &full, &rel, &base_url(&req, local));
        return respond(
            &mut stream,
            "200 OK",
            &[("Content-Type", "text/html; charset=utf-8")],
            html.as_bytes(),
            head,
        );
    }
    let extra = samsung_headers(ctx, &req, &rel, local);
    serve_file(&mut stream, &req, &full, meta.len(), head, &extra)
}

/// Samsung DLNA extensions on a media request:
/// `getCaptionInfo.sec: 1` -> `CaptionInfo.sec: <subtitle url>`,
/// `getMediaInfo.sec: 1` -> `MediaInfo.sec: SEC_Duration=<ms>;`.
fn samsung_headers(ctx: &Ctx, req: &Request, rel: &str, local: SocketAddr) -> Vec<(String, String)> {
    let mut extra = Vec::new();
    if mime_for(rel).1 != Kind::Video {
        return extra;
    }
    if req.header("getcaptioninfo.sec").is_some() {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let subs = safe_join(&ctx.root, parent)
            .map(|(dir, _)| read_entries(&dir, parent))
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.rel == rel)
            .map(|e| e.subs)
            .unwrap_or_default();
        if let Some(sub) = order_subs(&subs, name, ctx.sub_lang.as_deref()).first() {
            extra.push(("CaptionInfo.sec".into(), format!("{}/{}", base_url(req, local), url_encode_path(sub))));
        }
    }
    if req.header("getmediainfo.sec").is_some() {
        let dur = ctx.tc.zip(safe_join(&ctx.root, rel)).and_then(|(tc, (full, _))| tc.probe(&full)).and_then(|p| p.duration);
        if let Some(d) = dur {
            extra.push(("MediaInfo.sec".into(), format!("SEC_Duration={};", (d * 1000.0) as u64)));
        }
    }
    extra
}

/// `http://host:port` as seen by the client (Host header), falling back to the
/// local address the connection arrived on.
pub fn base_url(req: &Request, local: SocketAddr) -> String {
    match req.header("host") {
        Some(h) if !h.is_empty() && h.chars().all(|c| c.is_ascii_graphic()) => format!("http://{h}"),
        _ => format!("http://{local}"),
    }
}

/// Parse a single `Range: bytes=...` header. `None` = ignore and send the
/// whole file, `Some(Err)` = unsatisfiable (416).
fn parse_range(h: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (a, b) = first.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let r = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            Err(())
        } else {
            Ok((len.saturating_sub(n), len - 1))
        }
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
        if len == 0 || start >= len || start > end { Err(()) } else { Ok((start, end)) }
    };
    Some(r)
}

fn serve_file(stream: &mut TcpStream, req: &Request, path: &Path, len: u64, head: bool, extra: &[(String, String)]) -> io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (mime, _) = mime_for(&name);
    let mime = if is_samsung(req) { dlna::samsung_mime(mime) } else { mime };
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return respond(stream, "403 Forbidden", &[], b"Cannot open file", head),
    };

    let (status, start, end) = match req.header("range").and_then(|h| parse_range(h, len)) {
        None => ("200 OK", 0, len.saturating_sub(1)),
        Some(Ok((s, e))) => ("206 Partial Content", s, e),
        Some(Err(())) => {
            let cr = format!("bytes */{len}");
            return respond(stream, "416 Range Not Satisfiable", &[("Content-Range", &cr)], b"", head);
        }
    };
    let count = if len == 0 { 0 } else { end - start + 1 };

    let mut h = format!(
        "HTTP/1.1 {status}\r\nServer: RustMedia/{}\r\nConnection: close\r\nContent-Type: {mime}\r\nContent-Length: {count}\r\nAccept-Ranges: bytes\r\n\
         transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {}\r\n",
        env!("CARGO_PKG_VERSION"),
        dlna::CONTENT_FEATURES
    );
    if status.starts_with("206") {
        h.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    stream.write_all(h.as_bytes())?;
    if head || count == 0 {
        return stream.flush();
    }
    file.seek(SeekFrom::Start(start))?;
    let mut out = BufWriter::with_capacity(256 * 1024, &mut *stream);
    io::copy(&mut file.take(count), &mut out)?;
    out.flush()
}

pub struct Entry {
    pub name: String,
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    /// Relative paths of matching subtitle files (`Movie.srt`, `Movie.en.srt`, ...).
    pub subs: Vec<String>,
}

/// Visible children of a directory, folders first, naturally sorted.
pub fn read_entries(dir: &Path, rel: &str) -> Vec<Entry> {
    let mut v: Vec<Entry> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if is_hidden(&name) {
                    return None;
                }
                // fs::metadata follows symlinks, so linked folders work.
                let meta = fs::metadata(e.path()).ok()?;
                let rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                Some(Entry { name, rel, is_dir: meta.is_dir(), size: meta.len(), subs: Vec::new() })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| natural_cmp(&a.name, &b.name)));
    attach_subtitles(&mut v);
    v
}

/// For every video, find sibling subtitle files sharing its base name:
/// `Movie.mkv` matches `Movie.srt`, `Movie.en.srt`, `Movie.Malay.ass`.
/// An exact-name match comes first.
fn attach_subtitles(entries: &mut [Entry]) {
    let subs: Vec<(String, String)> = entries
        .iter()
        .filter(|e| !e.is_dir && is_subtitle(&e.name))
        .map(|e| (e.name.to_lowercase(), e.rel.clone()))
        .collect();
    if subs.is_empty() {
        return;
    }
    for e in entries.iter_mut().filter(|e| !e.is_dir && mime_for(&e.name).1 == Kind::Video) {
        let stem = e.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&e.name).to_lowercase();
        let mut found: Vec<(bool, &String)> = subs
            .iter()
            .filter_map(|(lname, rel)| {
                let sub_stem = lname.rsplit_once('.').map(|(s, _)| s)?;
                if sub_stem == stem {
                    Some((true, rel))
                } else if sub_stem.strip_prefix(&stem)?.starts_with('.') {
                    Some((false, rel))
                } else {
                    None
                }
            })
            .collect();
        found.sort_by_key(|(exact, _)| !exact);
        e.subs = found.into_iter().map(|(_, r)| r.clone()).collect();
    }
}

/// Recursively collect playable files (video/audio) under `dir`.
pub fn walk_media(dir: &Path, rel: &str, out: &mut Vec<Entry>, depth: usize) {
    if depth > 32 {
        return;
    }
    for e in read_entries(dir, rel) {
        if e.is_dir {
            walk_media(&dir.join(&e.name), &e.rel, out, depth + 1);
        } else if is_playable(&e.name) {
            out.push(e);
        }
    }
}

fn playlist(ctx: &Ctx, dir: &Path, rel: &str, base: &str, profile: Profile) -> String {
    let mut files = Vec::new();
    walk_media(dir, rel, &mut files, 0);
    files.sort_by(|a, b| natural_cmp(&a.rel, &b.rel));
    let mut s = String::from("#EXTM3U\n");
    for f in files {
        let title = f.rel.replace('/', " / ");
        let (path, transcoded) = ctx.play_path(&f.rel, profile);
        let title = match ctx.tc_state(&f.rel) {
            Some(st) if transcoded => format!("{title} [{}]", tc_note(&st)),
            _ => title,
        };
        s.push_str(&format!("#EXTINF:-1,{title}\n{base}{path}\n"));
    }
    s
}

fn human_size(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", units[i]) }
}

fn listing(ctx: &Ctx, dir: &Path, rel: &str, base: &str) -> String {
    let title = if rel.is_empty() { "/".to_string() } else { format!("/{rel}/") };
    let pl = if rel.is_empty() {
        format!("{base}/playlist.m3u")
    } else {
        format!("{base}/playlist.m3u?dir={}", url_encode_component(rel))
    };
    let mut rows = String::new();
    if !rel.is_empty() {
        rows.push_str("<li class=d><a href=\"../\">&#8592; ..</a></li>\n");
    }
    for e in read_entries(dir, rel) {
        let href = format!("/{}{}", url_encode_path(&e.rel), if e.is_dir { "/" } else { "" });
        let (class, label, size) = if e.is_dir {
            ("d", format!("{}/", escape_xml(&e.name)), String::new())
        } else {
            let c = if is_playable(&e.name) { "m" } else { "f" };
            (c, escape_xml(&e.name), human_size(e.size))
        };
        let copy = if !e.is_dir && is_playable(&e.name) {
            let (path, _) = ctx.play_path(&e.rel, Profile::Tv);
            let tag = ctx.tc_state(&e.rel).map(|st| format!(" ({})", tc_note(&st))).unwrap_or_default();
            format!(" <span class=u>{}{}{tag}</span>", escape_xml(base), escape_xml(&path))
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<li class={class}><a href=\"{}\">{label}</a> <small>{size}</small>{copy}</li>\n",
            escape_xml(&href)
        ));
    }
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>RustMedia {t}</title><style>
body{{font-family:system-ui,sans-serif;margin:0 auto;max-width:900px;padding:16px;background:#111;color:#eee}}
a{{color:#8cf;text-decoration:none}} li{{padding:6px 0;border-bottom:1px solid #333;list-style:none;word-break:break-all}}
ul{{padding:0}} small{{color:#999}} .d a{{color:#fc6;font-weight:600}} .m a{{color:#9f9}}
.u{{display:block;color:#777;font-size:12px;font-family:monospace}}
.box{{background:#222;padding:12px;border-radius:8px;font-size:14px}} code{{color:#9f9;word-break:break-all}}
</style></head><body><h2>RustMedia {t}</h2>
<div class=box>VLC &rarr; Media &rarr; Open Network Stream, paste this URL to play this folder as a playlist:<br><code>{pl}</code><br>
<a href="{pl_href}">Download playlist.m3u</a></div>
<ul>
{rows}</ul></body></html>"#,
        t = escape_xml(&title),
        pl = escape_xml(&pl),
        pl_href = escape_xml(&pl),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some(Ok((0, 99))));
        assert_eq!(parse_range("bytes=10-19", 100), Some(Ok((10, 19))));
        assert_eq!(parse_range("bytes=90-200", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("items=1-2", 100), None);
    }
}

#[cfg(test)]
mod samsung_tests {
    use super::*;

    #[test]
    fn preferred_sub_first() {
        let subs = vec!["d/Movie.srt".to_string(), "d/Movie.ms.srt".to_string()];
        assert_eq!(order_subs(&subs, "Movie.mkv", Some("ms"))[0], "d/Movie.ms.srt");
        assert_eq!(order_subs(&subs, "Movie.mkv", None)[0], "d/Movie.srt");
    }
}
