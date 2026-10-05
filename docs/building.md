# Building

```bash
sudo apt install libpulse-dev libasound2-dev pkg-config   # Debian/Ubuntu build dependencies
cargo build --release                                     # this machine: target/release/rtp-audio
cargo test
```

Windows `.exe` from Linux (needs `mingw-w64`):

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Runtime requirements and limitations

- The Linux binary links `libpulse.so.0` (`libpulse0` / `pulseaudio-libs`) and `libasound.so.2`
  (`libasound2` / `alsa-lib`) dynamically; both ship with nearly every desktop. It runs no other
  programs: no `pactl`, `parec` or shell scripts.
- The sender talks the PulseAudio protocol: it works with PulseAudio, and with PipeWire through
  `pipewire-pulse`. It never starts a sound server, installs anything or needs `sudo`.
- Capturing works on Linux only. The Windows build receives, and can `send --stdin`.
- On PipeWire, stopping restores the default output that was in use, not a preferred device that
  was disconnected at the time.
- The released Linux binary is built on Ubuntu 22.04 for x86_64, so it needs glibc 2.35 or newer.
