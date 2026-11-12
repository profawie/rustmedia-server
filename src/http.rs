//! Minimal threaded HTTP/1.1 server: directory listing, M3U playlists,
//! file streaming with Range (seek) support, plus the DLNA endpoints.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::dlna;
use crate::transcode::Transcoder;
use crate::util::*;

pub struct Ctx {
    pub root: PathBuf,
    pub dlna: Option<&'static dlna::Device>,
    pub tc: Option<&'static Transcoder>,
}

impl Ctx {
    /// URL path a client should use to play `rel`: the original file, or
    /// `/_tc/...` when it has to be transcoded for the TV.
    pub fn play_path(&self, rel: &str) -> (String, bool) {
        let needs_tc = self.tc.is_some_and(|tc| {
            mime_for(rel).1 == Kind::Video
                && safe_join(&self.root, rel).is_some_and(|(full, _)| tc.needs_transcode(&full))
        });
        let p = url_encode_path(rel);
        if needs_tc { (format!("/_tc/{p}"), true) } else { (format!("/{p}"), false) }
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
    let head = req.method == "HEAD";

    if req.path.starts_with("/_dlna/") {
        return match &ctx.dlna {
            Some(dev) => dlna::handle_http(ctx, dev, &req, &mut stream, local),
            None => respond(&mut stream, "404 Not Found", &[], b"DLNA disabled", head),
        };
    }

    if req.method != "GET" && !head {
        return respond(&mut stream, "405 Method Not Allowed", &[("Allow", "GET, HEAD")], b"", false);
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
        // Sibling subtitle files are muxed into the stream as tracks.
        let rel = percent_decode(rest, false);
        let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let subs: Vec<PathBuf> = full
            .parent()
            .map(|dir| read_entries(dir, parent))
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.rel == rel.trim_matches('/'))
            .map(|e| e.subs.iter().filter_map(|s| safe_join(&ctx.root, s).map(|(p, _)| p)).collect())
            .unwrap_or_default();
        stream.set_read_timeout(None)?;
        return tc.stream(&mut stream, &full, &subs, head);
    }

    if req.path == "/playlist.m3u" || req.path == "/playlist.m3u8" {
        let dir = req.query_param("dir").unwrap_or_default();
        let Some((full, rel)) = safe_join(&ctx.root, &dir) else {
            return respond(&mut stream, "400 Bad Request", &[], b"bad path", head);
        };
        let base = base_url(&req, local);
        let body = playlist(ctx, &full, &rel, &base);
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
        let html = listing(ctx, &full, &rel, &base_url(&req, local));
        return respond(
            &mut stream,
            "200 OK",
            &[("Content-Type", "text/html; charset=utf-8")],
            html.as_bytes(),
            head,
        );
    }
    serve_file(&mut stream, &req, &full, meta.len(), head)
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

fn serve_file(stream: &mut TcpStream, req: &Request, path: &Path, len: u64, head: bool) -> io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (mime, _) = mime_for(&name);
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

fn playlist(ctx: &Ctx, dir: &Path, rel: &str, base: &str) -> String {
    let mut files = Vec::new();
    walk_media(dir, rel, &mut files, 0);
    files.sort_by(|a, b| natural_cmp(&a.rel, &b.rel));
    let mut s = String::from("#EXTM3U\n");
    for f in files {
        let title = f.rel.replace('/', " / ");
        let (path, tc) = ctx.play_path(&f.rel);
        let title = if tc { format!("{title} [TV]") } else { title };
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
            let (path, tc) = ctx.play_path(&e.rel);
            let tag = if tc { " (transcoded for TV)" } else { "" };
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
