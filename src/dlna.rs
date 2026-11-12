//! DLNA / UPnP MediaServer so VLC (and smart TVs) find this server under
//! "Local Network" / "Universal Plug'n'Play" without typing any URL.
//!
//! Pieces: SSDP discovery (UDP multicast 239.255.255.250:1900), a device
//! description, and a ContentDirectory service whose Browse action maps
//! object IDs to relative paths ("0" = shared root).

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::http::{read_entries, respond, Ctx, Request};
use crate::transcode::CacheState;
use crate::util::*;

pub const CONTENT_FEATURES: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const CDS: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CMS: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

pub struct Device {
    pub uuid: String,
    pub name: String,
}

impl Device {
    pub fn new(name: String, seed: &str) -> Self {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h1 = DefaultHasher::new();
        ("rustmedia-a", seed).hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        ("rustmedia-b", seed).hash(&mut h2);
        let (a, b) = (h1.finish(), h2.finish());
        // Stable per computer + folder, so TVs don't collect duplicates.
        let uuid = format!(
            "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
            (a >> 32) as u32,
            (a >> 16) as u16,
            (a & 0xfff) as u16,
            (b >> 52) as u16 & 0xfff,
            b & 0xffff_ffff_ffff
        );
        Device { uuid, name }
    }

    fn usn(&self, nt: &str) -> String {
        if nt.starts_with("uuid:") {
            nt.to_string()
        } else {
            format!("uuid:{}::{nt}", self.uuid)
        }
    }

    fn notification_types(&self) -> Vec<String> {
        vec![
            "upnp:rootdevice".into(),
            format!("uuid:{}", self.uuid),
            DEVICE_TYPE.into(),
            CDS.into(),
            CMS.into(),
        ]
    }
}

fn server_header() -> String {
    format!("Windows/10 UPnP/1.0 RustMedia/{}", env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------- SSDP

/// Start SSDP: answer M-SEARCH on port 1900 and broadcast NOTIFY alive
/// messages periodically. Returns a warning string if the listener could not
/// be started (announcements still work in that case).
pub fn start_ssdp(dev: &'static Device, ip: Ipv4Addr, port: u16) -> Option<String> {
    let location = format!("http://{ip}:{port}/_dlna/description.xml");
    let mut warning = None;

    match bind_listener(ip) {
        Ok(sock) => {
            let location = location.clone();
            std::thread::spawn(move || listen(sock, dev, &location));
        }
        Err(e) => {
            warning = Some(format!("could not open SSDP port 1900 ({e}); sending announcements (NOTIFY) only."))
        }
    }

    std::thread::spawn(move || {
        let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(ip, 0)) else { return };
        let _ = sock.set_multicast_ttl_v4(4);
        let dest = SocketAddrV4::new(SSDP_ADDR, 1900);
        let mut round = 0u32;
        loop {
            for nt in dev.notification_types() {
                let msg = format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age=1800\r\n\
                     LOCATION: {location}\r\nNT: {nt}\r\nNTS: ssdp:alive\r\nSERVER: {}\r\nUSN: {}\r\n\r\n",
                    server_header(),
                    dev.usn(&nt)
                );
                let _ = sock.send_to(msg.as_bytes(), dest);
            }
            round += 1;
            // A few quick rounds at start-up (UDP is lossy), then every minute.
            std::thread::sleep(Duration::from_secs(if round < 3 { 2 } else { 60 }));
        }
    });
    warning
}

fn bind_listener(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Windows' own "SSDP Discovery" service also uses port 1900.
    s.set_reuse_address(true)?;
    s.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], 1900))))?;
    if s.join_multicast_v4(&SSDP_ADDR, &ip).is_err() {
        s.join_multicast_v4(&SSDP_ADDR, &Ipv4Addr::UNSPECIFIED)?;
    }
    Ok(s.into())
}

fn listen(sock: UdpSocket, dev: &Device, location: &str) {
    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, src)) = sock.recv_from(&mut buf) else { continue };
        let msg = String::from_utf8_lossy(&buf[..n]);
        if !msg.starts_with("M-SEARCH") {
            continue;
        }
        let st = msg
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case("st").then(|| v.trim().to_string())
            })
            .unwrap_or_default();
        let targets: Vec<String> = if st == "ssdp:all" {
            dev.notification_types()
        } else if dev.notification_types().iter().any(|t| *t == st) {
            vec![st]
        } else {
            continue;
        };
        for t in targets {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: {location}\r\n\
                 SERVER: {}\r\nST: {t}\r\nUSN: {}\r\nContent-Length: 0\r\n\r\n",
                server_header(),
                dev.usn(&t)
            );
            let _ = sock.send_to(resp.as_bytes(), src);
        }
    }
}

// ---------------------------------------------------------------- HTTP side

pub fn handle_http(
    ctx: &Ctx,
    dev: &Device,
    req: &Request,
    stream: &mut TcpStream,
    local: SocketAddr,
) -> io::Result<()> {
    let xml = [("Content-Type", "text/xml; charset=\"utf-8\"")];
    let head = req.method == "HEAD";
    match (req.method.as_str(), req.path.as_str()) {
        (_, "/_dlna/description.xml") => respond(stream, "200 OK", &xml, description(dev).as_bytes(), head),
        (_, "/_dlna/cds.xml") => respond(stream, "200 OK", &xml, CDS_SCPD.as_bytes(), head),
        (_, "/_dlna/cms.xml") => respond(stream, "200 OK", &xml, CMS_SCPD.as_bytes(), head),
        ("POST", "/_dlna/control/cds") | ("POST", "/_dlna/control/cms") => {
            let action = req
                .header("soapaction")
                .and_then(|a| a.trim_matches('"').rsplit_once('#').map(|(_, x)| x.to_string()))
                .unwrap_or_default();
            let body = String::from_utf8_lossy(&req.body);
            let base = format!("http://{local}");
            match control(ctx, &action, &body, &base, crate::http::is_samsung(req)) {
                Some(resp) => respond(stream, "200 OK", &xml, soap(&action, &resp).as_bytes(), false),
                None => respond(stream, "500 Internal Server Error", &xml, SOAP_FAULT.as_bytes(), false),
            }
        }
        ("SUBSCRIBE", _) => {
            // Eventing isn't implemented; accept so strict clients carry on.
            let sid = format!("uuid:{}-sub", dev.uuid);
            respond(stream, "200 OK", &[("SID", &sid), ("TIMEOUT", "Second-1800")], b"", false)
        }
        ("UNSUBSCRIBE", _) => respond(stream, "200 OK", &[], b"", false),
        _ => respond(stream, "404 Not Found", &[], b"", head),
    }
}

fn description(dev: &Device) -> String {
    let svc = |ty: &str, id: &str, short: &str| {
        format!(
            "<service><serviceType>{ty}</serviceType><serviceId>urn:upnp-org:serviceId:{id}</serviceId>\
             <SCPDURL>/_dlna/{short}.xml</SCPDURL><controlURL>/_dlna/control/{short}</controlURL>\
             <eventSubURL>/_dlna/event/{short}</eventSubURL></service>"
        )
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0" xmlns:sec="http://www.sec.co.kr/dlna">
<specVersion><major>1</major><minor>0</minor></specVersion>
<device><deviceType>{DEVICE_TYPE}</deviceType><friendlyName>{name}</friendlyName>
<manufacturer>RustMedia</manufacturer><modelName>RustMedia Server</modelName><modelNumber>{ver}</modelNumber>
<dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC><UDN>uuid:{uuid}</UDN>
<sec:ProductCap>smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec</sec:ProductCap>
<sec:X_ProductCap>smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec</sec:X_ProductCap>
<serviceList>{cds}{cms}</serviceList></device></root>"#,
        name = escape_xml(&dev.name),
        ver = env!("CARGO_PKG_VERSION"),
        uuid = dev.uuid,
        cds = svc(CDS, "ContentDirectory", "cds"),
        cms = svc(CMS, "ConnectionManager", "cms"),
    )
}

/// Extract the text of `<name>...</name>` (ignoring any namespace prefix on the tag).
fn xml_arg(body: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = body[from..].find('<').map(|i| i + from) {
        let rest = &body[i + 1..];
        let tag_end = rest.find(|c: char| c == '>' || c == '/' || c.is_whitespace())?;
        let tag = &rest[..tag_end];
        let local = tag.rsplit_once(':').map(|(_, l)| l).unwrap_or(tag);
        if local == name {
            let gt = rest.find('>')?;
            if rest[..gt].ends_with('/') {
                return Some(String::new());
            }
            let content = &rest[gt + 1..];
            let end = content.find("</")?;
            return Some(unescape_xml(content[..end].trim()));
        }
        from = i + 1;
    }
    None
}

/// Returns the inner response arguments (XML) for a SOAP action.
fn control(ctx: &Ctx, action: &str, body: &str, base: &str, native: bool) -> Option<String> {
    match action {
        "Browse" => {
            let id = xml_arg(body, "ObjectID").unwrap_or_else(|| "0".into());
            let flag = xml_arg(body, "BrowseFlag").unwrap_or_default();
            let start: usize = xml_arg(body, "StartingIndex").and_then(|v| v.parse().ok()).unwrap_or(0);
            let count: usize = xml_arg(body, "RequestedCount").and_then(|v| v.parse().ok()).unwrap_or(0);
            let (didl, returned, total) = browse(ctx, &id, flag == "BrowseMetadata", start, count, base, native)?;
            Some(format!(
                "<Result>{}</Result><NumberReturned>{returned}</NumberReturned>\
                 <TotalMatches>{total}</TotalMatches><UpdateID>1</UpdateID>",
                escape_xml(&didl)
            ))
        }
        "GetSystemUpdateID" => Some("<Id>1</Id>".into()),
        // Samsung extension: which containers to open for video/audio/photo.
        "X_GetFeatureList" => Some(format!(
            "<FeatureList>{}</FeatureList>",
            escape_xml(
                r#"<?xml version="1.0" encoding="UTF-8"?><Features xmlns="urn:schemas-upnp-org:av:avs" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="urn:schemas-upnp-org:av:avs http://www.upnp.org/schemas/av/avs.xsd"><Feature name="samsung.com_BASICVIEW" version="1"><container id="0" type="object.item.audioItem"/><container id="0" type="object.item.videoItem"/><container id="0" type="object.item.imageItem"/></Feature></Features>"#
            )
        )),
        "GetSearchCapabilities" => Some("<SearchCaps></SearchCaps>".into()),
        "GetSortCapabilities" => Some("<SortCaps></SortCaps>".into()),
        "GetProtocolInfo" => Some(
            "<Source>http-get:*:video/mp4:*,http-get:*:video/x-matroska:*,http-get:*:video/x-msvideo:*,\
             http-get:*:video/mpeg:*,http-get:*:video/mp2t:*,http-get:*:audio/mpeg:*,http-get:*:image/jpeg:*,\
             http-get:*:*:*</Source><Sink></Sink>"
                .into(),
        ),
        "GetCurrentConnectionIDs" => Some("<ConnectionIDs>0</ConnectionIDs>".into()),
        _ => None,
    }
}

fn soap(action: &str, inner: &str) -> String {
    let ns = if matches!(action, "GetProtocolInfo" | "GetCurrentConnectionIDs") { CMS } else { CDS };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action}Response xmlns:u="{ns}">{inner}</u:{action}Response></s:Body></s:Envelope>"#
    )
}

const DIDL_OPEN: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:dlna="urn:schemas-dlna-org:metadata-1-0/" xmlns:sec="http://www.sec.co.kr/">"#;

fn parent_id(rel: &str) -> String {
    match rel.rsplit_once('/') {
        Some((p, _)) => p.to_string(),
        None if rel.is_empty() => "-1".into(),
        None => "0".into(),
    }
}

fn shown(name: &str, is_dir: bool) -> bool {
    is_dir || matches!(mime_for(name).1, Kind::Video | Kind::Audio | Kind::Image)
}

fn container_xml(ctx: &Ctx, rel: &str, title: &str) -> String {
    let (full, _) = safe_join(&ctx.root, rel).unwrap_or_default();
    let count = read_entries(&full, rel).iter().filter(|e| shown(&e.name, e.is_dir)).count();
    let id = if rel.is_empty() { "0" } else { rel };
    format!(
        "<container id=\"{}\" parentID=\"{}\" restricted=\"1\" searchable=\"0\" childCount=\"{count}\">\
         <dc:title>{}</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>",
        escape_xml(id),
        escape_xml(&parent_id(rel)),
        escape_xml(title)
    )
}

/// Samsung players expect `video/x-mkv` rather than `video/x-matroska`.
pub fn samsung_mime(mime: &'static str) -> &'static str {
    if mime == "video/x-matroska" { "video/x-mkv" } else { mime }
}

/// `native`: the client plays the original files (Samsung), so no transcoding.
fn item_xml(ctx: &Ctx, rel: &str, name: &str, size: u64, subs: &[String], base: &str, native: bool) -> String {
    let (mime, kind) = mime_for(name);
    let class = match kind {
        Kind::Audio => "object.item.audioItem.musicTrack",
        Kind::Image => "object.item.imageItem.photo",
        _ => "object.item.videoItem",
    };
    let title = name.rsplit_once('.').map(|(t, _)| t).filter(|t| !t.is_empty()).unwrap_or(name);
    let tc_state = if native { None } else { ctx.tc_state(rel) };
    // Say in the title when a video can't be seeked yet.
    let title = match &tc_state {
        Some(st @ (CacheState::Converting(_) | CacheState::Waiting | CacheState::Failed)) => {
            format!("{title} ({})", crate::http::tc_note(st))
        }
        _ => title.to_string(),
    };
    // Subtitles. VLC turns every <res> with a text/* protocolInfo into a
    // subtitle track, so all of them are offered. Samsung TVs read
    // sec:CaptionInfoEx, which only holds one (the exact-name match).
    let (path, transcoded) = if native { (format!("/{}", url_encode_path(rel)), false) } else { ctx.play_path(rel) };
    // A transcoded stream already carries its subtitles as tracks.
    let subs = if transcoded { Vec::new() } else { crate::http::order_subs(subs, name, ctx.sub_lang.as_deref()) };
    let mut captions = String::new();
    let mut sub_res = String::new();
    for (i, sub) in subs.iter().enumerate() {
        let ext = sub.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        let url = escape_xml(&format!("{base}/{}", url_encode_path(sub)));
        if i == 0 {
            captions = format!(
                "<sec:CaptionInfo sec:type=\"{ext}\">{url}</sec:CaptionInfo><sec:CaptionInfoEx sec:type=\"{ext}\">{url}</sec:CaptionInfoEx>"
            );
        }
        // Samsung reads sec:CaptionInfo and could mistake extra <res> for
        // alternative video versions, so those are for other clients only.
        if !native {
            sub_res.push_str(&format!("<res protocolInfo=\"http-get:*:text/{ext}:*\">{url}</res>"));
        }
    }
    // Videos the TV can't decode point at the transcoding endpoint instead.
    let duration = match (kind, ctx.tc) {
        (Kind::Video, Some(tc)) => safe_join(&ctx.root, rel)
            .and_then(|(full, _)| tc.probe(&full))
            .and_then(|p| p.duration)
            .map(|d| format!(" duration=\"{}\"", crate::transcode::fmt_duration(d)))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let res_attrs = if let Some(CacheState::Ready(file)) = &tc_state {
        // Converted MP4: a normal seekable file.
        let size = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        format!("protocolInfo=\"http-get:*:video/mp4:{CONTENT_FEATURES}\" size=\"{size}\"{duration}")
    } else if transcoded {
        format!("protocolInfo=\"http-get:*:video/mp4:{}\"{duration}", crate::transcode::TC_FEATURES)
    } else {
        let mime = if native { samsung_mime(mime) } else { mime };
        format!("protocolInfo=\"http-get:*:{mime}:{CONTENT_FEATURES}\" size=\"{size}\"{duration}")
    };
    format!(
        "<item id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>{class}</upnp:class>\
         {captions}<res {res_attrs}>{}</res>{sub_res}</item>",
        escape_xml(rel),
        escape_xml(&parent_id(rel)),
        escape_xml(&title),
        escape_xml(&format!("{base}{path}"))
    )
}

fn browse(ctx: &Ctx, id: &str, metadata: bool, start: usize, count: usize, base: &str, native: bool) -> Option<(String, usize, usize)> {
    let rel_in = if id == "0" { "" } else { id };
    let (full, rel) = safe_join(&ctx.root, rel_in)?;
    let meta = fs::metadata(&full).ok()?;
    let mut didl = String::from(DIDL_OPEN);

    if metadata {
        let name = rel.rsplit('/').next().unwrap_or("").to_string();
        if meta.is_dir() {
            let title = if rel.is_empty() { "RustMedia".to_string() } else { name };
            didl.push_str(&container_xml(ctx, &rel, &title));
        } else {
            let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            let parent_dir = full.parent().unwrap_or(&ctx.root);
            let subs = read_entries(parent_dir, parent)
                .into_iter()
                .find(|e| e.rel == rel)
                .map(|e| e.subs)
                .unwrap_or_default();
            didl.push_str(&item_xml(ctx, &rel, &name, meta.len(), &subs, base, native));
        }
        didl.push_str("</DIDL-Lite>");
        return Some((didl, 1, 1));
    }

    if !meta.is_dir() {
        return None;
    }
    let entries: Vec<_> = read_entries(&full, &rel).into_iter().filter(|e| shown(&e.name, e.is_dir)).collect();
    let total = entries.len();
    let take = if count == 0 { usize::MAX } else { count };
    let mut returned = 0;
    for e in entries.into_iter().skip(start).take(take) {
        if e.is_dir {
            didl.push_str(&container_xml(ctx, &e.rel, &e.name));
        } else {
            didl.push_str(&item_xml(ctx, &e.rel, &e.name, e.size, &e.subs, base, native));
        }
        returned += 1;
    }
    didl.push_str("</DIDL-Lite>");
    Some((didl, returned, total))
}

const SOAP_FAULT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>No such object or action</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;

const CDS_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>Browse</name><argumentList>
<argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
<argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>
<argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>
<argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>
<argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>
<argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>
<argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>
</argumentList></action>
<action><name>X_GetFeatureList</name><argumentList><argument><name>FeatureList</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Featurelist</relatedStateVariable></argument></argumentList></action>
<action><name>GetSystemUpdateID</name><argumentList><argument><name>Id</name><direction>out</direction><relatedStateVariable>SystemUpdateID</relatedStateVariable></argument></argumentList></action>
<action><name>GetSearchCapabilities</name><argumentList><argument><name>SearchCaps</name><direction>out</direction><relatedStateVariable>SearchCapabilities</relatedStateVariable></argument></argumentList></action>
<action><name>GetSortCapabilities</name><argumentList><argument><name>SortCaps</name><direction>out</direction><relatedStateVariable>SortCapabilities</relatedStateVariable></argument></argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType><allowedValueList><allowedValue>BrowseMetadata</allowedValue><allowedValue>BrowseDirectChildren</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Featurelist</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>SystemUpdateID</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>SearchCapabilities</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>SortCapabilities</name><dataType>string</dataType></stateVariable>
</serviceStateTable></scpd>"#;

const CMS_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>GetProtocolInfo</name><argumentList>
<argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
<argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentConnectionIDs</name><argumentList>
<argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
</serviceStateTable></scpd>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let b = r#"<s:Body><u:Browse xmlns:u="x"><ObjectID>Filem/A &amp; B</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>50</RequestedCount><SortCriteria/></u:Browse></s:Body>"#;
        assert_eq!(xml_arg(b, "ObjectID").as_deref(), Some("Filem/A & B"));
        assert_eq!(xml_arg(b, "RequestedCount").as_deref(), Some("50"));
        assert_eq!(xml_arg(b, "SortCriteria").as_deref(), Some(""));
        assert_eq!(xml_arg(b, "Browse").is_some(), true);
    }

    #[test]
    fn uuid_shape() {
        let d = Device::new("x".into(), "seed");
        assert_eq!(d.uuid.len(), 36);
        assert_eq!(d.uuid, Device::new("y".into(), "seed").uuid);
    }
}
