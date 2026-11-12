# RustMedia

Media server kecil untuk Windows, ditulis sepenuhnya dalam Rust. Jalankan
`rustmedia.exe` dalam folder video anda, dan VLC di laptop, telefon atau TV
(dalam Wi-Fi yang sama) boleh pilih dan main video dari folder itu.

## Cara guna

1. Salin `rustmedia.exe` ke folder video (contoh `D:\Filem`), kemudian
   double-click, atau dari terminal:

   ```
   cd D:\Filem
   rustmedia.exe
   ```

2. Kali pertama, Windows Firewall akan bertanya. Pilih **Allow** untuk
   rangkaian **Private**. (Jika Wi-Fi rumah ditetapkan sebagai *Public*,
   tukar ke *Private* di Settings > Network, atau benarkan juga Public.)

3. Program akan papar alamat seperti:

   ```
   http://192.168.1.23:8080/playlist.m3u      <- semua video (playlist)
   http://192.168.1.23:8080/                  <- senarai fail (pelayar web)
   ```

4. Di VLC:
   - **Cara A (taip URL):** Media > Open Network Stream, masukkan
     `http://<ip>:8080/playlist.m3u`. Semua video (termasuk subfolder)
     muncul dalam playlist; pilih mana-mana dan main. Seek/lompat masa
     berfungsi.
   - **Cara B (tanpa taip, sesuai untuk TV):** buka *Local Network* /
     *Universal Plug'n'Play* (UPnP) dan pilih **RustMedia - <folder> (<PC>)**.
     Anda boleh browse folder dan pilih video terus.

Senarai fail juga boleh dibuka dengan pelayar web di `http://<ip>:8080/`.
Setiap folder ada playlist sendiri: `http://<ip>:8080/playlist.m3u?dir=Nama%20Folder`.

## Pilihan

```
rustmedia.exe [FOLDER] [--port N] [--name NAMA] [--no-dlna]
```

| Pilihan | Maksud |
| --- | --- |
| `FOLDER` | Folder dikongsi (lalai: folder semasa) |
| `--port N` | Port HTTP (lalai 8080, cuba 8081.. jika sibuk) |
| `--name NAMA` | Nama di VLC > Local Network |
| `--no-dlna` | Matikan penemuan UPnP/DLNA |

## Kekangan

- Laptop dan TV mesti dalam rangkaian yang sama. Sesetengah router
  "Guest Wi-Fi" atau "AP isolation" menghalang peranti saling bercakap.
- Tiada transcoding: fail dihantar seperti asal. Sama ada video boleh dimain
  bergantung pada VLC di TV (VLC biasanya sokong hampir semua format, tetapi
  video 4K/HEVC berat mungkin tersekat pada TV lama atau Wi-Fi lemah).
- Subtitle berasingan (`.srt`) tidak dimuat automatik melalui URL. Gunakan
  subtitle dalam fail (contohnya MKV) atau tambah sendiri dalam VLC.
- Program hanya untuk rangkaian rumah: tiada kata laluan, sesiapa dalam
  Wi-Fi yang sama boleh lihat folder yang dikongsi.
- Laptop jangan *sleep* semasa menonton.

## Bina dari kod

```
cargo build --release                                     # di Windows
cargo build --release --target x86_64-pc-windows-gnu      # cross-compile dari Linux (perlu mingw-w64)
```
