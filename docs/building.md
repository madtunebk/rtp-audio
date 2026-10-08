# Building

One command builds rtp-audio complete, with its window (`rtp-audio --gui`):

```bash
# Debian/Ubuntu build dependencies; and Node.js 20 or newer with npm (https://nodejs.org)
sudo apt install libpulse-dev libasound2-dev libdbus-1-dev pkg-config cmake \
                 libwebkit2gtk-4.1-dev libxdo-dev libssl-dev librsvg2-dev
cargo build --release          # target/release/rtp-audio: the CLI and --gui
cargo test
```

The window's page is built by npm and its program by cargo, from the build scripts: nothing to
run before. On Linux, rtp-audio itself still links no WebKit or GTK: the window is a separate
program carried inside it and started only for `--gui` (`readelf -d target/release/rtp-audio`).

Without the window, e.g. on a server with no Node or WebKit development files, and kept apart so it
never replaces the complete one:

```bash
CARGO_TARGET_DIR=target/headless cargo build --release --no-default-features
```

Windows `.exe` from Linux (needs `mingw-w64`):

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Built this way (mingw), the exe needs `WebView2Loader.dll` beside it for the window; the release
(MSVC) links it in, and links the C runtime statically (`.cargo/config.toml`), so it needs no
Visual C++ Redistributable.

## Runtime requirements and limitations

- The Linux binary links `libpulse.so.0` (`libpulse0` / `pulseaudio-libs`) and `libasound.so.2`
  (`libasound2` / `alsa-lib`), plus `libdbus-1.so.3` (`libdbus-1-3` / `dbus-libs`) dynamically.
  D-Bus lets direct audio threads request real-time scheduling through RTKit; if promotion is
  unavailable, playback continues and reports the failure. These libraries ship with most desktops. It runs no other
  programs (no `pactl`, `parec` or shell scripts), except the window for `--gui`, which alone needs
  WebKitGTK 4.1 (`libwebkit2gtk-4.1-0`).
- Opus (for the browser mode) is compiled into the binary from source, which is why building
  needs `cmake`; running needs no Opus library.
- The sender talks the PulseAudio protocol: it works with PulseAudio, and with PipeWire through
  `pipewire-pulse`. It never starts a sound server, installs anything or needs `sudo`.
- Capturing works on Linux only. The Windows build receives, and can `send --stdin`.
- On PipeWire, stopping restores the default output that was in use, not a preferred device that
  was disconnected at the time.
- The released Linux binary is built on Ubuntu 22.04 for x86_64, so it needs glibc 2.35 or newer.
