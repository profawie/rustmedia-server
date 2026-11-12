# RustMedia

A small media server for Windows, written entirely in Rust. Run
`rustmedia.exe` inside your video folder and VLC on your laptop, phone or TV
(on the same Wi-Fi) can browse and play the videos in that folder.

## Usage

1. Copy `rustmedia.exe` into your video folder (e.g. `D:\Movies`) and
   double-click it, or from a terminal:

   ```
   cd D:\Movies
   rustmedia.exe
   ```

2. The first time, Windows Firewall will ask for permission. Choose
   **Allow** for **Private** networks. (If your home Wi-Fi is set to
   *Public*, switch it to *Private* in Settings > Network, or allow Public too.)

3. The program prints addresses like:

   ```
   http://192.168.1.23:8080/playlist.m3u      <- all videos (playlist)
   http://192.168.1.23:8080/                  <- file list (web browser)
   ```

4. In VLC:
   - **Option A (type a URL):** Media > Open Network Stream, enter
     `http://<ip>:8080/playlist.m3u`. Every video (including subfolders)
     appears in the playlist; pick one and play. Seeking works.
   - **Option B (no typing, best for TVs):** open *Local Network* /
     *Universal Plug'n'Play* (UPnP) and choose **RustMedia - <folder> (<PC>)**.
     You can browse folders and pick a video directly.

The file list can also be opened in a web browser at `http://<ip>:8080/`.
Each folder has its own playlist: `http://<ip>:8080/playlist.m3u?dir=Folder%20Name`.

## Subtitles

Put the subtitle file next to the video with the same name:

```
Movie.mkv
Movie.srt        (or Movie.en.srt, Movie.ms.srt; .srt, .vtt, .ass and .ssa work)
```

- **Option B (UPnP / Local Network):** the subtitle is attached to the video
  automatically. If it isn't shown, open VLC's subtitle menu and pick the
  track (VLC loads one subtitle file per video, the exact-name match first).
- **Option A (playlist URL):** VLC does not accept subtitle files from a
  playlist over the network, so use Option B, use subtitles embedded in the
  file (e.g. MKV), or on the laptop use Subtitle > Add Subtitle File.

## Options

```
rustmedia.exe [FOLDER] [--port N] [--name NAME] [--no-dlna]
```

| Option | Meaning |
| --- | --- |
| `FOLDER` | Folder to share (default: current directory) |
| `--port N` | HTTP port (default 8080, tries 8081.. if busy) |
| `--name NAME` | Name shown in VLC > Local Network |
| `--no-dlna` | Disable UPnP/DLNA discovery |

## Limitations

- The laptop and TV must be on the same network. Some routers' "Guest Wi-Fi"
  or "AP isolation" stop devices from talking to each other.
- No transcoding: files are sent as-is. Whether a video plays depends on VLC
  on the TV (VLC supports almost every format, but heavy 4K/HEVC video may
  stutter on old TVs or weak Wi-Fi).
- Separate subtitle files only work through Option B (UPnP); see below.
- Intended for home networks only: there is no password, so anyone on the
  same Wi-Fi can see the shared folder.
- Don't let the laptop sleep while watching.

## Building

```
cargo build --release                                     # on Windows
cargo build --release --target x86_64-pc-windows-gnu      # cross-compile from Linux (needs mingw-w64)
```
