# riv

A fast, minimal image viewer for Windows. One portable executable, no installation.

## Features

- Instant startup and folder navigation with a configurable preload cache
- HDR support: scRGB FP16 pipeline, content peak detection, tone mapping
  to the display's capability, and dithered SDR output
- Color management: embedded ICC profiles, PQ/HLG sources, Windows Advanced Color
- Animation playback (GIF, APNG, animated WebP) with pause, frame stepping, and speed control
- Browse images inside archives (Windows 11 23H2+, using the in-box libarchive)
- Per-extension file associations, fully reversible with no registry leftovers
- Small: a single statically linked executable of about 7 MB
- Portable: settings are stored in `riv.json` next to the executable
- Exception: running elevated (as administrator) is not supported and is blocked at startup

## Supported formats

Some formats need a codec extension from the Microsoft Store. The missing
extension is named in the error message when a file can't be decoded:

| Format | Required extension |
|---|---|
| HEIC / HEIF | HEVC Video Extensions (optional; falls back to the built-in decoder below) |
| AVIF | AV1 Video Extension |
| JPEG XL | JPEG XL Image Extension |
| WebP (still) | WebP Image Extensions |
| Camera RAW | Raw Image Extension |

Decoded by built-in codecs (no dependency to install):

| Format | Decoder |
|---|---|
| HEIC / HEIF | libheif + libde265, used when the Windows extension above is absent |
| SVG / SVGZ | resvg |
| EXR | OpenEXR |
| APNG | png crate |
| Animated WebP | libwebp |

Decoded by the in-box Windows Imaging Component codecs:

| Format | Notes |
|---|---|
| PNG, JPEG, GIF, BMP, ICO, TIFF | |
| DDS | BC1–BC3 (DXT1–5) compressed only |

Archives browsable as image folders (Windows 11 23H2+):
zip, 7z, rar, tar, and the comic book variants cbz / cbr / cb7 / cbt.

## Requirements

- Windows 10/11, x86-64 (AVX2-capable CPU)
- Direct3D 11 capable GPU

## Building

The build cross-compiles from Linux (including WSL) to `x86_64-pc-windows-msvc`.

Prerequisites:

- Rust with the `x86_64-pc-windows-msvc` target
- LLVM: clang-cl, lld-link, llvm-lib, llvm-rc, llvm-mt
- A Windows CRT + SDK splat from [xwin](https://github.com/Jake-Shadle/xwin)
  in `~/.xwin` (override the location with `XWIN_ROOT`)
- CMake and Ninja, for the static codec dependencies

```sh
./deps/build_deps.sh   # one-time static build of the C/C++ codecs
cargo build --release
```

## Acknowledgments

Inspired by [qView](https://github.com/jurplel/qView).
[mpv](https://github.com/mpv-player/mpv) and
[libplacebo](https://code.videolan.org/videolan/libplacebo) served as references
for the window handling and the HDR pipeline.

## License

GPL-3.0-only (see [LICENSE](LICENSE)).

Statically linked third-party components and their licenses are listed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). The application icon is derived
from [Fluent UI System Icons](https://github.com/microsoft/fluentui-system-icons) (MIT).
