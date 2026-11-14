//! RustMedia: share the current folder over the LAN so VLC (PC, phone, TV)
//! can play the videos. HTTP + M3U playlist + Range seeking + DLNA/UPnP.

mod dlna;
mod http;
mod transcode;
mod util;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

const HELP: &str = "\
RustMedia - share a video folder with VLC over the local network (LAN)

Usage: rustmedia.exe [FOLDER] [--port N] [--name NAME] [--no-dlna]
                     [--no-transcode] [--encoder NAME] [--sub-lang TAG]

  FOLDER       Folder to share (default: current directory)
  --port N     HTTP port (default: 8080; tries the next port if busy)
  --name NAME  Name shown in VLC > Local Network (UPnP)
  --no-dlna    Disable UPnP/DLNA discovery
  --no-transcode  Never transcode, always send files as-is
  --sub-lang TAG  Subtitle shown first; burned into the video for Samsung
                  TVs, from Movie.TAG.srt
                  (e.g. --sub-lang ms for Movie.ms.srt)
  --encoder NAME  ffmpeg H.264 encoder (default: libx264; GPU options:
                  h264_nvenc, h264_qsv, h264_amf)

Transcoding: put ffmpeg.exe and ffprobe.exe next to rustmedia.exe (or on
PATH). Videos a TV can't decode (HEVC/x265, 10-bit, VP9, AV1) are then
converted to H.264 while streaming. Other files are sent as-is.
  -h, --help   Show this help
";

struct Args {
    dir: Option<PathBuf>,
    port: Option<u16>,
    name: Option<String>,
    dlna: bool,
    transcode: bool,
    encoder: String,
    sub_lang: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { dir: None, port: None, name: None, dlna: true, transcode: true, encoder: "libx264".into(), sub_lang: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" | "/?" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-p" | "--port" => {
                let v = it.next().ok_or("--port needs a number")?;
                a.port = Some(v.parse().map_err(|_| format!("invalid port: {v}"))?);
            }
            "--name" => a.name = Some(it.next().ok_or("--name needs a value")?),
            "--no-dlna" => a.dlna = false,
            "--no-transcode" => a.transcode = false,
            "--encoder" => a.encoder = it.next().ok_or("--encoder needs a value")?,
            "--sub-lang" => a.sub_lang = Some(it.next().ok_or("--sub-lang needs a value, e.g. ms")?),
            s if s.starts_with('-') => return Err(format!("unknown option: {s}\n\n{HELP}")),
            s => a.dir = Some(PathBuf::from(s)),
        }
    }
    Ok(a)
}

/// Folder for converted videos: `.rustmedia-cache` inside the shared folder
/// (hidden from listings), or the temp folder if that isn't writable.
fn cache_dir(root: &std::path::Path) -> PathBuf {
    let preferred = root.join(".rustmedia-cache");
    let writable = |d: &std::path::Path| {
        std::fs::create_dir_all(d).is_ok() && {
            let probe = d.join(".write-test");
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        }
    };
    if writable(&preferred) {
        return preferred;
    }
    let fallback = std::env::temp_dir().join("rustmedia-cache");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Keep the console window open when started by double-click so errors can be read.
fn fail(msg: &str) -> ! {
    eprintln!("\nERROR: {msg}");
    eprintln!("Press Enter to exit...");
    let _ = std::io::stdin().read_line(&mut String::new());
    std::process::exit(1);
}

fn main() {
    let args = parse_args().unwrap_or_else(|e| fail(&e));
    let dir = match args.dir {
        Some(d) => d,
        None => std::env::current_dir().unwrap_or_else(|e| fail(&format!("cannot read current directory: {e}"))),
    };
    let root = dir
        .canonicalize()
        .unwrap_or_else(|e| fail(&format!("folder '{}' does not exist: {e}", dir.display())));
    if !root.is_dir() {
        fail(&format!("'{}' is not a folder", root.display()));
    }
    // canonicalize() on Windows yields `\\?\C:\...`; strip it for display.
    let shown_root = root.display().to_string().trim_start_matches(r"\\?\").to_string();

    // Bind HTTP.
    let (listener, port) = match args.port {
        Some(p) => match TcpListener::bind(("0.0.0.0", p)) {
            Ok(l) => (l, p),
            Err(e) => fail(&format!("cannot open port {p}: {e}")),
        },
        None => (8080..8100)
            .find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok().map(|l| (l, p)))
            .unwrap_or_else(|| fail("no free port between 8080-8099")),
    };

    // Initial scan for the summary.
    let mut media = Vec::new();
    http::walk_media(&root, "", &mut media, 0);
    let total_size: u64 = media.iter().map(|e| e.size).sum();

    let ip = util::lan_ip();
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "PC".into());
    let folder_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| shown_root.clone());

    let device = if args.dlna {
        let name = args.name.clone().unwrap_or_else(|| format!("RustMedia - {folder_name} ({host})"));
        let dev: &'static dlna::Device = Box::leak(Box::new(dlna::Device::new(name, &format!("{host}|{shown_root}"))));
        Some(dev)
    } else {
        None
    };

    let tc: Option<&'static transcode::Transcoder> = if args.transcode {
        transcode::Transcoder::detect(args.encoder.clone(), cache_dir(&root)).map(|t| &*Box::leak(Box::new(t)))
    } else {
        None
    };
    let ctx = Arc::new(http::Ctx {
        root: root.clone(),
        dlna: device,
        tc,
        sub_lang: args.sub_lang.clone(),
        clients: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    if let Some(tc) = tc {
        tc.start_worker();
        // Probe every video in the background and queue the ones the TV
        // can't play for conversion, in folder order.
        let ctx = ctx.clone();
        let videos: Vec<String> = media
            .iter()
            .filter(|e| util::mime_for(&e.name).1 == util::Kind::Video)
            .map(|e| e.rel.clone())
            .collect();
        std::thread::spawn(move || {
            for rel in videos {
                let Some((full, _)) = util::safe_join(&ctx.root, &rel) else { continue };
                if tc.needs_transcode(&full) {
                    tc.enqueue(full, ctx.sibling_subs(&rel), false);
                }
            }
        });
    }

    println!("============================================================");
    println!(" RustMedia {}  -  media server for VLC", env!("CARGO_PKG_VERSION"));
    println!("============================================================");
    println!(" Folder   : {shown_root}");
    println!(
        " Found    : {} video/audio files ({:.2} GB)",
        media.len(),
        total_size as f64 / 1_073_741_824.0
    );
    println!();
    println!(" In VLC: Media > Open Network Stream, enter:");
    println!();
    println!("   http://{ip}:{port}/playlist.m3u      <- all videos (playlist)");
    println!("   http://{ip}:{port}/                  <- file list (web browser)");
    println!();
    println!(" Each player gets its own version of a video:");
    println!("   Samsung TV      : original file; with --sub-lang, a copy with those");
    println!("                     subtitles burned into the picture");
    println!("   TV with VLC     : H.264 copy when needed, all subtitles as tracks");
    println!("   PC/Mac with VLC : original file, full quality");
    println!(" A Mac or another PC using the playlist link: add ?for=pc to it.");
    println!();
    if let Some(dev) = device {
        match dlna::start_ssdp(dev, ip, port) {
            None => {
                println!(" UPnP/DLNA: on. In VLC (TV/phone) open Local Network /");
                println!("   Universal Plug'n'Play and choose \"{}\".", dev.name);
            }
            Some(w) => println!(" UPnP/DLNA: {w}"),
        }
        println!();
    }
    match tc {
        Some(t) => {
            println!(" Transcoding: on ({}, encoder {}).", t.ffmpeg_path().display(), t.encoder());
            println!("   HEVC/x265, 10-bit, VP9 and AV1 videos are converted to H.264 MP4");
            println!("   in the background, one at a time, into:");
            println!("   {}", t.cache_dir().display().to_string().trim_start_matches(r"\\?\"));
            println!("   Once converted they can be seeked. Before that they still play,");
            println!("   streamed live, but without seeking.");
        }
        None if args.transcode => {
            println!(" Transcoding: off (ffmpeg.exe + ffprobe.exe not found next to");
            println!("   rustmedia.exe or on PATH). HEVC videos may not play on TVs.");
        }
        None => println!(" Transcoding: off (--no-transcode)."),
    }
    println!();
    if ip.is_loopback() {
        println!(" WARNING: no network detected. Connect to Wi-Fi first.");
    }
    println!(" The laptop and TV must be on the same Wi-Fi/network.");
    println!(" If Windows Firewall asks, choose 'Allow' (Private network).");
    println!(" Press Ctrl+C to stop.");
    println!("============================================================");

    http::run(ctx, listener);
}
