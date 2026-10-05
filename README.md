# rtp-audio

Hear a remote Linux desktop's sound on your own computer, over the network. One small program:

- `rtp-audio send` on the **Linux** computer whose sound you want (e.g. the VNC server): captures
  its sound and sends it as RTP over UDP.
- `rtp-audio` on the computer you sit at (**Windows** or **Linux**): listens on UDP port 46000 and
  plays what arrives. On Windows it's a single `.exe`, no DLLs or installer.

It also plays standard RTP L16 streams (PulseAudio `module-rtp-send`, PipeWire `module-rtp-sink`,
`ffmpeg -f rtp -acodec pcm_s16be`) at 48 kHz stereo, so other senders work too.

## Security: use a LAN or a private VPN

The sound is **not** part of noVNC: it travels as its own unencrypted UDP stream, straight from
the sender to the receiver, so NGINX and its HTTPS do not protect it. Anyone on the path can
listen in. Use it on your home or office LAN, or through a private VPN (WireGuard, Tailscale,
...) and send to the receiver's VPN address. Do not open port 46000 to the internet.

## Optional: sound for your VNC desktop

**1. Start the receiver** on the computer you sit at.

Windows: run `rtp-audio.exe`. The first time, Windows asks about the firewall: allow it on
**private** networks.

Linux (needs `libasound2`, already installed on desktops):

```bash
./rtp-audio
sudo ufw allow 46000/udp   # only if you use the ufw firewall
```

It prints `Listening on UDP port 46000` and, once sound arrives, `Receiving from ...`.

**2. Find the receiver's IP address**: `ipconfig` on Windows, `ip -4 addr` on Linux (the LAN or
VPN address, e.g. `192.168.1.20`). From WSL, the Windows host is
`ip route | awk '/default/ {print $3}'` (with mirrored networking, `127.0.0.1`).

**3. Start the sender** on the Linux desktop, as the desktop's user (not with `sudo`):

```bash
./rtp-audio send 192.168.1.20:46000
```

This sends everything the desktop plays: it adds an output called "RTP Audio", makes it the
default and moves apps that are already playing onto it. Press **Ctrl+C** to stop; sound
switches back to where it was.

To send one source instead (a microphone, or a copy of a real output), without changing any
outputs:

```bash
./rtp-audio sources                                   # names and IDs
./rtp-audio send 192.168.1.20:46000 --source alsa_output.pci-0000_00_1f.3.analog-stereo.monitor
```

IDs can change when the sound server restarts; names don't.

### If something goes wrong

- **Crackles or dropouts**: give the receiver a bigger buffer, e.g. `rtp-audio --latency 120`.
- **"sender already running"**: only one sender runs per user. Stop the other one (Ctrl+C in its
  terminal, or the `kill` command the message shows). `sources` and `--help` work meanwhile.
- **"cannot connect to the sound server"**: the sender needs PulseAudio, or PipeWire with its
  PulseAudio server (package `pipewire-pulse`), running for your user; the message says how to
  start it. On a VNC desktop it starts with the desktop session.
- **Sound stuck on "RTP Audio"** after the sender was killed (e.g. `kill -9` or a crash): run
  `rtp-audio send` again. It first switches back what the killed one left behind.
- **`error while loading shared libraries: libpulse.so.0`**: install the PulseAudio client
  library, e.g. `sudo apt install libpulse0` (Debian/Ubuntu) or `sudo dnf install
  pulseaudio-libs` (Fedora).

## Options

```
rtp-audio [receive] [--port 46000] [--latency 60] [--rate 48000] [--channels 2]
rtp-audio sources
rtp-audio send HOST:PORT [--source NAME_OR_ID]
rtp-audio send HOST:PORT --stdin [--rate 48000] [--channels 2]   # raw s16be PCM from stdin
```

## Build

```bash
sudo apt install libpulse-dev libasound2-dev pkg-config   # Debian/Ubuntu build dependencies
cargo build --release                                     # this machine
cargo build --release --target x86_64-pc-windows-gnu      # Windows .exe from Linux (needs mingw-w64)
```

### Runtime requirements and limitations

- The Linux binary links `libpulse.so.0` (`libpulse0` / `pulseaudio-libs`) and `libasound.so.2`
  (`libasound2` / `alsa-lib`) dynamically; both ship with nearly every desktop. It runs no other
  programs: no `pactl`, `parec` or shell scripts.
- The sender talks the PulseAudio protocol: it works with PulseAudio, and with PipeWire through
  `pipewire-pulse`. It never starts a sound server.
- Capturing works on Linux only; the Windows build receives (and can `send --stdin`).
- On PipeWire, switching back restores the default output that was in use. If your preferred
  default was a device that was disconnected at the time (e.g. Bluetooth headphones), pick it
  again in the sound settings after reconnecting it.

## How it works

The sender asks the sound server for 48 kHz stereo 16-bit audio from "RTP Audio"'s monitor (or
the chosen source) and sends it in 5 ms RTP packets. It records each change it makes to the
sound settings in `$XDG_RUNTIME_DIR/rtp-audio/` before making it, and marks its output with a
random ID, so it only ever undoes its own changes, even after being killed.

On the receiver, packets go into a jitter buffer (60 ms by default) that absorbs network
hiccups; lost packets become silence. The sound card pulls from it through a small resampler,
which also plays up to 0.5% faster or slower to keep the buffer level, since the sender's and
the sound card's clocks never run at exactly the same speed.
