# PiPie

<p align="left">
  <a href="https://github.com/lifeFxxker/pipie/releases"><img src="https://img.shields.io/github/v/release/lifeFxxker/pipie?color=black&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-1f1f1f" alt="Platform">
  <img src="https://img.shields.io/badge/language-Rust-1f1f1f" alt="Rust">
  <img src="https://img.shields.io/badge/license-MIT-1f1f1f" alt="License">
</p>

**English · [简体中文](README.md)**

A cursor-centred radial launcher for Windows. Press a mouse side button and the ring opens at the pointer; hover an app and its windows fan out beside it, with live previews. One file, no runtime, no installer.

## Demo

![Interaction demo](design/preview-demo.gif)

Hover an app to fan out its windows; drag an inner-ring icon to reorder.

## Resource usage

Measured on Windows 11 (build 26200), i3-12100F (4C/8T), 16 GB RAM, release build of `PiPie.exe` (745 KB).

| State | CPU | Memory (working set) | Commit | Handles | Threads |
| --- | --- | --- | --- | --- | --- |
| Idle (tray only, 20 s average) | 0.01% | 0.2 MB | 11.4 MB | 370 | 13 |
| Ring open (5 s average) | 2.15% | 4.9 MB | 13.1 MB | 366 | 12 |

- CPU is a share of the whole machine, not of one core.
- The 0.2 MB idle working set is because the app hands its pages back to the OS while hidden (`SetProcessWorkingSetSize`). The real footprint is the 11.4 MB commit; the cost is one minor page-in on the first summon.
- The 2% while open comes from software rendering and sampling the desktop behind the glass — frames are redrawn continuously.

## Features

- **Opens at the pointer** — no travelling to a screen corner.
- **Two rings** — apps on the inner ring, their open windows as capsules on the outer ring, hugging the app instead of scattering around the circle.
- **Live window previews** — DWM thumbnails, including the last frame of a minimised window. Not screenshots.
- **Drag to pin** — shortcuts, `.exe` files, folders and files can be dropped on the inner ring; reorder, rename and remove in the same view.
- **Up to 24 apps** — the inner ring shows 12 at a time; scroll the wheel to keep going, tiles cross-fade at the seam.
- **One file, no dependencies** — a 745 KB `PiPie.exe`. No .NET, no runtime.
- **Several ways to summon** — mouse side button by default; middle button, long right-press, or a recorded key combo.

## Screenshots

| Inner ring | Outer ring — windows of one app |
| :---: | :---: |
| ![Inner ring](design/preview-ring.png) | ![Outer ring](design/preview-chips.png) |
| Running apps get a dot under their icon. | Hover an app and its windows fan out as capsules beside it. |

| Live preview | Settings |
| :---: | :---: |
| ![Live preview](design/preview-live.png) | ![Settings](design/preview-apps.png) |
| Hover a capsule for a live DWM thumbnail. | Flat, paper-like preferences. |

> The binary is unsigned; SmartScreen will ask for "More info → Run anyway" on first launch. Download only from this repository's Releases.

## Install

Grab `pipie-vX.Y.Z-win64.zip` from [Releases](https://github.com/lifeFxxker/pipie/releases), extract anywhere, run `PiPie.exe`. Nothing to install.

### Build from source

You need Windows 10/11 x64, [Rust](https://rustup.rs/), the MSVC toolchain (`winget install Microsoft.VisualStudio.BuildTools`) and the Windows SDK (used to embed the icon; skipped if missing).

```bash
cargo build --release      # -> target/release/PiPie.exe
python package.py          # -> dist/pipie-v<version>-win64.zip
```

> **Do not change `lto = "thin"` in `Cargo.toml`.** With fat LTO (`"true"`) the current toolchain miscompiles this project's `unsafe` FFI and the app dies on startup (`0xC0000409`).

## Usage

- **Summon** — hold the mouse side button; the ring opens at the pointer. Release to launch the highlighted app, `Esc` or any other key to cancel.
- **Pin** — drag a shortcut, `.exe`, folder or file onto an inner-ring tile.
- **Adopt a window** — drag a window onto the ring to attach it to its app.
- **Preview** — hover an app to fan out its windows; hover a capsule for a live thumbnail.
- **Scroll** — once there are more apps than one screen, the wheel moves the inner ring, one tile per notch.
- **Settings** — right-click the tray icon → Settings.

## Configuration

`%APPDATA%\PiPie\settings.ini`, created on first run; normally edited through the settings page. Logs go to `%LOCALAPPDATA%\PiPie\logs\`. Upgrading from the old Pigo build copies the legacy config automatically on first launch.

## License

[MIT](LICENSE). Use it, modify it, redistribute it, sell it — including in closed-source products — as long as you keep the copyright and permission notice.
If paid services are added later, the client stays open source; see [LICENSING.md](LICENSING.md) for the boundary.
