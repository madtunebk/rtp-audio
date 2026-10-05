# Troubleshooting

**Crackles or dropouts.** Give the receiver a bigger buffer, e.g. `rtp-audio --latency 120`.
Every few seconds the receiver prints what went wrong (lost packets, dropouts, skips).

**No sound, and the receiver never prints `Receiving from ...`.** Check that the sender uses
the receiver's address and port, that both are on the same LAN or VPN, and that the receiver's
firewall allows UDP port 46000.

**"sender already running".** Only one sender runs per user. Stop the other one (Ctrl+C in its
terminal, or the `kill` command the message shows). `rtp-audio sources` and `--help` work
meanwhile.

**"cannot connect to the sound server".** The sender needs PulseAudio, or PipeWire with its
PulseAudio server (package `pipewire-pulse`), running for your user. The message says how to
start it. On a VNC desktop it normally starts with the desktop session. Run the sender as that
user, not with `sudo`.

**"unknown source".** Run `rtp-audio sources` and copy the name. IDs change when the sound
server restarts.

**Sound stuck on "RTP Audio" after the sender was killed** (e.g. `kill -9` or a crash). Run
`rtp-audio send` again: it first switches back what the killed one left behind.

**`error while loading shared libraries: libpulse.so.0`.** Install the PulseAudio client
library: `sudo apt install libpulse0` (Debian/Ubuntu) or `sudo dnf install pulseaudio-libs`
(Fedora). It works with PipeWire too.

**My preferred default output changed after stopping (PipeWire).** Stopping restores the output
that was in use. If your preferred default was a device that was disconnected at the time (e.g.
Bluetooth headphones), pick it again in the sound settings after reconnecting it.
