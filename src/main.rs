//! RustMedia: share the current folder over the LAN so VLC (PC, phone, TV)
//! can play the videos. HTTP + M3U playlist + Range seeking + DLNA/UPnP.

mod dlna;
mod http;
mod util;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

const HELP: &str = "\
RustMedia - kongsi folder video ke VLC melalui rangkaian (LAN)

Guna:  rustmedia.exe [FOLDER] [--port N] [--name NAMA] [--no-dlna]

  FOLDER       Folder untuk dikongsi (lalai: folder semasa)
  --port N     Port HTTP (lalai: 8080; cuba port seterusnya jika sibuk)
  --name NAMA  Nama yang dipaparkan di VLC > Local Network (UPnP)
  --no-dlna    Matikan penemuan UPnP/DLNA
  -h, --help   Papar bantuan ini
";

struct Args {
    dir: Option<PathBuf>,
    port: Option<u16>,
    name: Option<String>,
    dlna: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { dir: None, port: None, name: None, dlna: true };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" | "/?" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-p" | "--port" => {
                let v = it.next().ok_or("--port perlukan nombor")?;
                a.port = Some(v.parse().map_err(|_| format!("port tidak sah: {v}"))?);
            }
            "--name" => a.name = Some(it.next().ok_or("--name perlukan nilai")?),
            "--no-dlna" => a.dlna = false,
            s if s.starts_with('-') => return Err(format!("pilihan tidak dikenali: {s}\n\n{HELP}")),
            s => a.dir = Some(PathBuf::from(s)),
        }
    }
    Ok(a)
}

/// Keep the console window open when started by double-click so errors can be read.
fn fail(msg: &str) -> ! {
    eprintln!("\nRALAT: {msg}");
    eprintln!("Tekan Enter untuk keluar...");
    let _ = std::io::stdin().read_line(&mut String::new());
    std::process::exit(1);
}

fn main() {
    let args = parse_args().unwrap_or_else(|e| fail(&e));
    let dir = match args.dir {
        Some(d) => d,
        None => std::env::current_dir().unwrap_or_else(|e| fail(&format!("tidak dapat baca folder semasa: {e}"))),
    };
    let root = dir
        .canonicalize()
        .unwrap_or_else(|e| fail(&format!("folder '{}' tidak wujud: {e}", dir.display())));
    if !root.is_dir() {
        fail(&format!("'{}' bukan folder", root.display()));
    }
    // canonicalize() on Windows yields `\\?\C:\...`; strip it for display.
    let shown_root = root.display().to_string().trim_start_matches(r"\\?\").to_string();

    // Bind HTTP.
    let (listener, port) = match args.port {
        Some(p) => match TcpListener::bind(("0.0.0.0", p)) {
            Ok(l) => (l, p),
            Err(e) => fail(&format!("port {p} tidak dapat dibuka: {e}")),
        },
        None => (8080..8100)
            .find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok().map(|l| (l, p)))
            .unwrap_or_else(|| fail("tiada port kosong antara 8080-8099")),
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

    let ctx = Arc::new(http::Ctx {
        root: root.clone(),
        dlna: device,
    });

    println!("============================================================");
    println!(" RustMedia {}  -  media server untuk VLC", env!("CARGO_PKG_VERSION"));
    println!("============================================================");
    println!(" Folder   : {shown_root}");
    println!(
        " Dijumpai : {} fail video/audio ({:.2} GB)",
        media.len(),
        total_size as f64 / 1_073_741_824.0
    );
    println!();
    println!(" Di VLC: Media > Open Network Stream, masukkan:");
    println!();
    println!("   http://{ip}:{port}/playlist.m3u      <- semua video (playlist)");
    println!("   http://{ip}:{port}/                  <- senarai fail (pelayar web)");
    println!();
    if let Some(dev) = device {
        match dlna::start_ssdp(dev, ip, port) {
            None => {
                println!(" UPnP/DLNA: aktif. Di VLC (TV/telefon) buka Local Network /");
                println!("   Universal Plug'n'Play, pilih \"{}\".", dev.name);
            }
            Some(w) => println!(" UPnP/DLNA: {w}"),
        }
        println!();
    }
    if ip.is_loopback() {
        println!(" AMARAN: tiada rangkaian dikesan. Sambung ke Wi-Fi dahulu.");
    }
    println!(" Laptop dan TV mesti dalam Wi-Fi/rangkaian yang sama.");
    println!(" Jika Windows Firewall bertanya, pilih 'Allow' (Private network).");
    println!(" Tekan Ctrl+C untuk berhenti.");
    println!("============================================================");

    http::run(ctx, listener);
}
