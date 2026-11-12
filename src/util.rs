//! Small helpers: percent-encoding, XML/HTML escaping, MIME types, sorting,
//! safe path joining and LAN IP detection. Std-only.

use std::cmp::Ordering;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};

/// Percent-encode a relative path for use in a URL. `/` is kept as separator.
pub fn url_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Percent-encode a single query component (also encodes `/`).
pub fn url_encode_component(s: &str) -> String {
    url_encode_path(s).replace('/', "%2F")
}

/// Decode `%XX` sequences. Invalid escapes are kept literally.
/// When `plus_is_space` is set (query strings), `+` becomes a space.
pub fn percent_decode(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if b == b'+' && plus_is_space {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape text for HTML and XML (both content and attribute values).
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Video,
    Audio,
    Image,
    Subtitle,
    Other,
}

/// MIME type and kind from a file name's extension.
pub fn mime_for(name: &str) -> (&'static str, Kind) {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    use Kind::*;
    match ext.as_str() {
        "mp4" | "m4v" => ("video/mp4", Video),
        "mkv" => ("video/x-matroska", Video),
        "webm" => ("video/webm", Video),
        "avi" => ("video/x-msvideo", Video),
        "mov" => ("video/quicktime", Video),
        "wmv" => ("video/x-ms-wmv", Video),
        "flv" => ("video/x-flv", Video),
        "ts" | "m2ts" | "mts" => ("video/mp2t", Video),
        "mpg" | "mpeg" | "vob" => ("video/mpeg", Video),
        "3gp" => ("video/3gpp", Video),
        "ogv" => ("video/ogg", Video),
        "divx" => ("video/divx", Video),
        "rmvb" | "rm" => ("application/vnd.rn-realmedia", Video),
        "mp3" => ("audio/mpeg", Audio),
        "flac" => ("audio/flac", Audio),
        "m4a" => ("audio/mp4", Audio),
        "aac" => ("audio/aac", Audio),
        "wav" => ("audio/wav", Audio),
        "ogg" | "oga" => ("audio/ogg", Audio),
        "opus" => ("audio/opus", Audio),
        "wma" => ("audio/x-ms-wma", Audio),
        "jpg" | "jpeg" => ("image/jpeg", Image),
        "png" => ("image/png", Image),
        "gif" => ("image/gif", Image),
        "webp" => ("image/webp", Image),
        "bmp" => ("image/bmp", Image),
        "srt" => ("application/x-subrip", Subtitle),
        "vtt" => ("text/vtt", Subtitle),
        "ass" | "ssa" => ("text/x-ssa", Subtitle),
        "sub" => ("text/plain", Subtitle),
        "txt" | "log" | "nfo" => ("text/plain; charset=utf-8", Other),
        "html" | "htm" => ("text/html; charset=utf-8", Other),
        "pdf" => ("application/pdf", Other),
        _ => ("application/octet-stream", Other),
    }
}

pub fn is_playable(name: &str) -> bool {
    matches!(mime_for(name).1, Kind::Video | Kind::Audio)
}

/// Entries we never show: dotfiles and Windows system clutter.
pub fn is_hidden(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    name.starts_with('.')
        || l == "$recycle.bin"
        || l == "system volume information"
        || l == "desktop.ini"
        || l == "thumbs.db"
}

/// Case-insensitive "natural" ordering so "Episode 2" sorts before "Episode 10".
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Join a `/`-separated relative path (already decoded) onto `root`,
/// rejecting anything that could escape it. Returns the full path and the
/// normalised relative path ("" for the root).
pub fn safe_join(root: &Path, rel: &str) -> Option<(PathBuf, String)> {
    let mut path = root.to_path_buf();
    let mut parts: Vec<&str> = Vec::new();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains('\\') || seg.contains(':') || seg.contains('\0') {
            return None;
        }
        path.push(seg);
        parts.push(seg);
    }
    Some((path, parts.join("/")))
}

/// The IPv4 address of the interface used for the default route (normally
/// the Wi-Fi / Ethernet adapter). No packet is actually sent.
pub fn lan_ip() -> Ipv4Addr {
    let probe = || -> Option<Ipv4Addr> {
        let s = UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("8.8.8.8:80").ok()?;
        match s.local_addr().ok()?.ip() {
            IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
            _ => None,
        }
    };
    probe().unwrap_or(Ipv4Addr::LOCALHOST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let s = "Filem/Abang Adik (2023) [1080p] #1 50%.mkv";
        let e = url_encode_path(s);
        assert!(!e.contains(' ') && !e.contains('#') && !e.contains('['));
        assert_eq!(percent_decode(&e, false), s);
        assert_eq!(percent_decode("a%2", false), "a%2");
        assert_eq!(percent_decode("%E2%9C%93", false), "\u{2713}");
    }

    #[test]
    fn natural() {
        assert_eq!(natural_cmp("Ep 2", "Ep 10"), Ordering::Less);
        assert_eq!(natural_cmp("b", "A"), Ordering::Greater);
    }

    #[test]
    fn join_rejects_traversal() {
        let r = Path::new("/srv");
        assert!(safe_join(r, "../etc").is_none());
        assert!(safe_join(r, "a/../../x").is_none());
        assert!(safe_join(r, "C:/Windows").is_none());
        assert!(safe_join(r, "a\\..\\b").is_none());
        assert_eq!(safe_join(r, "/a//b/").unwrap().1, "a/b");
    }
}
