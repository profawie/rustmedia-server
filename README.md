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
  automatically. Every matching file becomes a track (e.g. `Movie.srt` and
  `Movie.ms.srt`); pick the one you want in VLC under Subtitle > Sub Track.
- **Option A (playlist URL):** VLC does not accept separate subtitle files
  from a playlist over the network. Videos that are transcoded (see below)
  carry their subtitles inside the stream, so they work with both options.
  For other videos use Option B, subtitles embedded in the file, or on the
  laptop Subtitle > Add Subtitle File.

## Transcoding (HEVC / x265 videos on TVs)

Many TVs can't decode HEVC (x265), 10-bit, VP9 or AV1 video, so VLC shows a
black screen. RustMedia can convert those videos to H.264 while streaming:

1. Download ffmpeg for Windows, e.g. the "release essentials" zip from
   https://www.gyan.dev/ffmpeg/builds/ (or `winget install Gyan.FFmpeg`).
2. Copy `ffmpeg.exe` and `ffprobe.exe` from its `bin` folder next to
   `rustmedia.exe` (or have them on PATH).
3. Start `rustmedia.exe`. It prints `Transcoding: on`.

RustMedia checks each video with ffprobe. Files that are already 8-bit
H.264 are sent as-is (with seeking). The others are converted on the fly:

- video to H.264 (max 1080p), audio tracks to AAC stereo;
- subtitles go inside the stream as tracks: text subtitles embedded in the
  file plus `Movie.srt`, `Movie.ms.srt`, ... (named Malay, English, ...);
- seeking (jumping ahead) is not possible in a transcoded video;
- it uses the laptop's CPU. If it can't keep up, try a GPU encoder:
  `--encoder h264_nvenc` (NVIDIA), `h264_qsv` (Intel) or `h264_amf` (AMD).

In the playlist and file list, transcoded videos are marked `[TV]`.

## Options

```
rustmedia.exe [FOLDER] [--port N] [--name NAME] [--no-dlna] [--no-transcode] [--encoder NAME]
```

| Option | Meaning |
| --- | --- |
| `FOLDER` | Folder to share (default: current directory) |
| `--port N` | HTTP port (default 8080, tries 8081.. if busy) |
| `--name NAME` | Name shown in VLC > Local Network |
| `--no-dlna` | Disable UPnP/DLNA discovery |
| `--no-transcode` | Never transcode, always send files as-is |
| `--encoder NAME` | ffmpeg H.264 encoder (default `libx264`) |

## Limitations

- The laptop and TV must be on the same network. Some routers' "Guest Wi-Fi"
  or "AP isolation" stop devices from talking to each other.
- Without ffmpeg, files are sent as-is and whether a video plays depends on
  the TV's decoder (see Transcoding). Transcoded videos can't be seeked.
- Picture-based subtitles (PGS, VobSub) are dropped from transcoded videos.
- Intended for home networks only: there is no password, so anyone on the
  same Wi-Fi can see the shared folder.
- Don't let the laptop sleep while watching.

## Building

On Windows, run the build script (needs Rust from https://rustup.rs):

```
.\build.ps1                  # run tests, build, copy rustmedia.exe here
.\build.ps1 -SkipTests       # build only
.\build.ps1 -Output D:\Movies # also copy the exe into your video folder
.\build.ps1 -Gnu             # use the GNU toolchain if MSVC Build Tools aren't installed
```

If PowerShell refuses to run scripts: `powershell -ExecutionPolicy Bypass -File .\build.ps1`.

Or by hand:

```
cargo build --release                                     # on Windows
cargo build --release --target x86_64-pc-windows-gnu      # cross-compile from Linux (needs mingw-w64)
```
