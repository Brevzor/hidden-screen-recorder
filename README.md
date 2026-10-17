<p align="center"><img src="docs/images/logo.svg" width="80" alt="Hidden Screen Recorder"></p>

<h1 align="center">Hidden Screen Recorder</h1>
<p align="center">Record your screen. Capture your audio. Keep it simple.</p>

<p align="center">
  <img src="https://img.shields.io/badge/Windows-10%20%2F%2011-0078D4?style=flat-square" alt="Windows 10 / 11">
  <img src="https://img.shields.io/badge/Rust-native-DEA584?style=flat-square&amp;logo=rust&amp;logoColor=white" alt="Built with Rust">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-35B67A?style=flat-square" alt="MIT license"></a>
</p>

<p align="center">
  <a href="https://github.com/brevzor/hidden-screen-recorder/releases"><img src="https://img.shields.io/badge/Download-Windows_x64-1677FF?style=for-the-badge&amp;logo=github&amp;logoColor=white" alt="Download"></a>
  <a href="https://github.com/brevzor/hidden-screen-recorder"><img src="https://img.shields.io/badge/Star-on_GitHub-F5C542?style=for-the-badge&amp;logo=github&amp;logoColor=171717" alt="Star on GitHub"></a>
</p>

<p align="center"><strong>English</strong> · <a href="README.ru.md">Русский</a></p>

<p align="center"><img src="docs/images/app-screenshot.png" width="1100" alt="Hidden Screen Recorder interface in English"></p>

A native Windows screen recorder built with Rust. Saves H.264 / AAC video to MP4 without FFmpeg, Python, or virtual audio drivers.

## Features

- **Screen capture** — select a monitor, frame rate, bitrate, and cursor visibility.
- **Audio** — record system sound and a chosen microphone, with separate volume controls.
- **Hotkeys** — `Ctrl+F9` to start, `Ctrl+F10` to stop. Both are customizable.
- **Hidden window** — optionally hide the window and taskbar icon while recording.
- **Auto-stop** — set a time limit or record without one.
- **EN / RU** — follows the Windows display language; switch beside the GitHub icon. Your choice is saved. Other system languages default to English.

## Download & run

1. Get `hidden-screen-recorder.exe` from [Releases](https://github.com/brevzor/hidden-screen-recorder/releases).
2. Choose your display, audio sources, and output folder.
3. Press **Start recording**. Stop with `Ctrl+F10` to finalize the MP4.

**Windows 10 / 11 x64.** No Rust installation required for the prebuilt exe. If no binary release is available, build below.

The exe can be renamed before launch. Hiding the window does not hide the process from Task Manager. Capture exclusion requires Windows 10 2004+; GDI is used when the initial Windows Graphics Capture check fails.

## Build

Requires Rust stable, Visual Studio C++ Build Tools, and Windows SDK (`x86_64-pc-windows-msvc`).

```powershell
git clone https://github.com/brevzor/hidden-screen-recorder.git
cd hidden-screen-recorder
cargo build --locked --release
```

Output: `target\release\hidden-screen-recorder.exe`.

## Built with

| Libraries | Purpose |
| :--- | :--- |
| [eframe / egui](https://docs.rs/eframe/0.33) | Native UI and OpenGL rendering |
| [windows-capture](https://docs.rs/windows-capture/2.0.1) | Screen capture and Media Foundation encoding |
| [wasapi](https://docs.rs/wasapi/0.25) | System audio loopback and microphone input |
| [windows](https://docs.rs/windows/0.62), [raw-window-handle](https://docs.rs/raw-window-handle/0.6) | Win32 APIs and window handles |
| [serde](https://docs.rs/serde/1), [serde_json](https://docs.rs/serde_json/1) | Settings serialization |
| [rfd](https://docs.rs/rfd/0.15), [directories](https://docs.rs/directories/6) | Folder picker and system paths |
| [chrono](https://docs.rs/chrono/0.4), [tempfile](https://docs.rs/tempfile/3) | Timestamps and atomic settings saves |
| [resvg](https://docs.rs/resvg/0.48), [winresource](https://docs.rs/winresource/0.1) | Build-time SVG rendering and Windows icons |

Versions: [Cargo.toml](Cargo.toml) · Locked dependencies: [Cargo.lock](Cargo.lock).

## Settings

Settings: `%APPDATA%\hidden-screen-recorder\config\settings.json`. Legacy settings are imported automatically when the new file is absent.

Logs are off by default. Enable them in the UI to write `hidden-screen-recorder-debug.log` beside the exe. Use the main keyboard for hotkey assignment; NumPad with NumLock on is rejected.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

[Report a bug](https://github.com/brevzor/hidden-screen-recorder/issues) · [CI](.github/workflows/ci.yml)

[MIT](LICENSE) © Hidden Screen Recorder contributors.
