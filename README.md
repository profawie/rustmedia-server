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

## What each player gets

RustMedia recognises the player and sends the version that suits it:

| Player | Gets |
|---|---|
| Samsung TV / Smart Monitor | The original file. With `--sub-lang`, a copy with those subtitles burned into the picture. |
| VLC on a TV (Google TV, Android TV) | An H.264 copy when the TV can't decode the original (HEVC, 10-bit, ...), with every subtitle as a selectable track. |
| VLC on a PC or Mac | Always the original file, full quality; its subtitles are offered as tracks. |

Players are told apart by what they report over UPnP (Samsung, Windows/macOS,
or Linux/Android), and the result is remembered per device, so the
console shows a line such as `Client 192.168.1.20: Samsung TV ...` the
first time each one connects. VLC on the laptop running RustMedia counts
as a PC. Using the playlist link from a Mac or another PC, add
`?for=pc` (or `?for=tv`) to choose the version yourself:

```
http://192.168.1.4:8080/playlist.m3u?for=pc
```

## Samsung TVs and Smart Monitors (no VLC)

Samsung Tizen TVs and Smart Monitors (e.g. M7) have a built-in DLNA player:

1. Start `rustmedia.exe` on the laptop (same Wi-Fi as the monitor).
2. On the monitor: **Source** (or Home > **Connected Devices**), choose
   **RustMedia - <folder> (<PC>)**, browse to the video and play it.

Samsung players decode HEVC, MKV and 10-bit video themselves, so they get
the original files (no conversion, seeking works).

Samsung players ignore `.srt` files next to the video, and on the
Smart Monitor M7 they also show nothing for subtitle tracks added to a
file. So with `--sub-lang`, RustMedia (with ffmpeg available) makes a copy
of the video with that language's subtitles **burned into the picture**:

```
rustmedia.exe --sub-lang ms      # burns Movie.ms.srt into Movie.mkv for Samsung
```

The video is re-encoded to H.264, so this takes a while (roughly the
length of the film or less, depending on the laptop; `--encoder
h264_nvenc` / `h264_qsv` / `h264_amf` is much faster on a GPU). It starts
the first time the monitor opens the folder; meanwhile the video's name
ends with "(ms subtitles: burning in N%, open again later)" and the
original plays. The copies are kept in `.rustmedia-cache`. Burned-in
subtitles can't be switched off on the monitor. Once the copy exists,
the monitor always gets it, even if it replays an older link (for example
after switching input to the PC and back).

Without `--sub-lang`, videos with subtitle files get a copy with those
files added as subtitle tracks (no re-encoding), which some Samsung models
show in **Subtitle Settings**.

Without ffmpeg the monitor gets one subtitle file through Samsung's
`CaptionInfo.sec` extension, which some models ignore.

`--no-transcode` turns ffmpeg off completely, so it also stops the
subtitle copies for Samsung.

## Transcoding (HEVC / x265 videos on TVs)

Many TVs can't decode HEVC (x265), 10-bit, VP9 or AV1 video, so VLC shows a
black screen. RustMedia can convert those videos to H.264 while streaming:

1. Download ffmpeg for Windows, e.g. the "release essentials" zip from
   https://www.gyan.dev/ffmpeg/builds/ (or `winget install Gyan.FFmpeg`).
2. Copy `ffmpeg.exe` and `ffprobe.exe` from its `bin` folder next to
   `rustmedia.exe` (or have them on PATH).
3. Start `rustmedia.exe`. It prints `Transcoding: on`.

RustMedia checks each video with ffprobe. Files that are already 8-bit
H.264 are sent as-is. The others are **converted in the background**, one
at a time, into a new MP4 in `.rustmedia-cache` inside the shared folder
(or the temp folder if that one is read-only). The console shows progress:

```
 Converting for TV: Movie.mkv
 Converting Movie.mkv: 30%
 Ready for TV (seekable): Movie.mkv (24.5 min)
```

- Once a video is converted it plays from the MP4 and **seeking works**.
- A video you open before it is converted moves to the front of the queue
  and is streamed live meanwhile, without seeking. Its name shows
  `(TV, converting 40%, no seeking yet)`; refresh the list in VLC later.
- The MP4 has H.264 video (max 1080p), all audio tracks as AAC stereo, and
  the subtitles as tracks: text subtitles embedded in the file plus
  `Movie.srt`, `Movie.ms.srt`, ... (named Malay, English, ...).
- Converted files are reused on the next start. They are made again if the
  video or its subtitle files change. You can delete `.rustmedia-cache` at
  any time to free space (a 2-hour 1080p film takes roughly 2-4 GB).
- Converting uses the CPU (at low priority). To make it faster, try a GPU
  encoder: `--encoder h264_nvenc` (NVIDIA), `h264_qsv` (Intel) or
  `h264_amf` (AMD).

In the playlist and file list, these videos are marked `[TV]`.

## Options

```
rustmedia.exe [FOLDER] [--port N] [--name NAME] [--no-dlna] [--no-transcode] [--encoder NAME] [--sub-lang TAG]
```

| Option | Meaning |
| --- | --- |
| `FOLDER` | Folder to share (default: current directory) |
| `--port N` | HTTP port (default 8080, tries 8081.. if busy) |
| `--name NAME` | Name shown in VLC > Local Network |
| `--no-dlna` | Disable UPnP/DLNA discovery |
| `--no-transcode` | Never transcode, always send files as-is |
| `--encoder NAME` | ffmpeg H.264 encoder (default `libx264`) |
| `--sub-lang TAG` | Subtitle listed first; burned into the picture for Samsung (`ms` = `Movie.ms.srt`) |

## Limitations

- The laptop and TV must be on the same network. Some routers' "Guest Wi-Fi"
  or "AP isolation" stop devices from talking to each other.
- Without ffmpeg, files are sent as-is and whether a video plays depends on
  the TV's decoder (see Transcoding). A video can't be seeked until its
  conversion has finished.
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
.\build.ps1 -Gnu             # force the GNU toolchain (picked automatically without Visual Studio)
```

If PowerShell refuses to run scripts: `powershell -ExecutionPolicy Bypass -File .\build.ps1`.

Or by hand:

```
cargo build --release                                     # on Windows
cargo build --release --target x86_64-pc-windows-gnu      # cross-compile from Linux (needs mingw-w64)
```
