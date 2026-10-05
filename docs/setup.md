# Setup: sound for your VNC desktop

This adds sound to a remote Linux desktop (e.g. one you use through noVNC). You run two things:

- the **receiver** on the computer you sit at (Windows or Linux), which plays the sound;
- the **sender** on the remote Linux desktop, which captures its sound and sends it.

Download both from the [releases page](https://github.com/madtunebk/rtp-audio/releases), or
[build them yourself](building.md).

## Before you start: use a LAN or a private VPN

The sound is **not** part of noVNC. It travels as its own unencrypted UDP stream, straight from
the sender to the receiver, so NGINX and its HTTPS do not protect it, and anyone on the path can
listen in. Use it on your home or office LAN, or through a private VPN (WireGuard, Tailscale,
...) and send to the receiver's VPN address. Do not open port 46000 to the internet.

## 1. Start the receiver

On the computer you sit at.

**Windows:** run `rtp-audio.exe`. The first time, Windows asks about the firewall: allow it on
**private** networks.

**Linux** (needs `libasound2`, already installed on desktops):

```bash
./rtp-audio
sudo ufw allow 46000/udp   # only if you use the ufw firewall
```

It prints `Listening on UDP port 46000` and, once sound arrives, `Receiving from ...`.

## 2. Find the receiver's IP address

- Windows: `ipconfig`
- Linux: `ip -4 addr`

Use the LAN or VPN address, e.g. `192.168.1.20`. From WSL, the Windows host is
`ip route | awk '/default/ {print $3}'` (with mirrored networking, `127.0.0.1`).

## 3. Start the sender

On the remote Linux desktop, as the desktop's user (not with `sudo`):

```bash
./rtp-audio send 192.168.1.20:46000
```

This sends everything the desktop plays: it adds an output called "RTP Audio", makes it the
default and moves apps that are already playing onto it. Press **Ctrl+C** to stop; sound
switches back to where it was.

### Sending one source only

To send a single source (a microphone, or a copy of a real output) without changing any outputs:

```bash
./rtp-audio sources                                   # names and IDs
./rtp-audio send 192.168.1.20:46000 --source alsa_output.pci-0000_00_1f.3.analog-stereo.monitor
```

A "Monitor of ..." source is a copy of what that output plays, and it keeps playing on the
desktop too. IDs can change when the sound server restarts; names don't.

## All options

```
rtp-audio [receive] [--port 46000] [--latency 60] [--rate 48000] [--channels 2]
rtp-audio sources
rtp-audio send HOST:PORT [--source NAME_OR_ID]
rtp-audio send HOST:PORT --stdin [--rate 48000] [--channels 2]   # raw s16be PCM from stdin
```

`--latency` is the receiver's buffer in milliseconds. Something went wrong? See
[troubleshooting](troubleshooting.md).
