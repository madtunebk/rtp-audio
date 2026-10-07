# Troubleshooting

**Crackles or dropouts.** Give the receiver a bigger buffer, e.g. `rtp-audio --latency 120`.
Every few seconds the receiver prints what went wrong (lost packets, dropouts, skips, sound card
underruns).

**"A buffer underrun or overrun occurred" on an output.** This is a hardware xrun, separate
from an empty network jitter queue. Direct ALSA outputs use 40 ms periods and an 80 ms hardware
ring; `--latency` changes the application queue, not that ring. The receiver counts startup
xruns too. Check per-output `card` versus `dropouts`/`skips` in `--json`, and inspect the actual
ALSA parameters under `/proc/asound/card*/pcm*p/sub0/hw_params`. Audio callbacks use bounded
queues and request real-time scheduling through RTKit. A scheduling failure is reported, not
hidden; check that RTKit/system D-Bus is available before assuming real-time scheduling worked.

**The sound cuts out regularly, every minute or so.** Something may keep restarting the sound
server on the sending desktop. Check with
`journalctl --user -u pipewire --since "-10min" | grep -E "Started|Stopped"`. One cause seen on
Ubuntu 26.04: a VNC server set to start GNOME, which can't run there (GNOME 50 has no X11
session), so it fails, restarts, and takes PipeWire down each time. Use XFCE (or another X11
desktop) in the VNC server; GNOME is reached through its own RDP.

**"Ignoring sound from …: already playing …".** Two senders are sending to the same receiver.
It keeps playing the first, and takes the other over once the first has been quiet for a second.

**"several outputs are called …".** Two outputs share that name (e.g. two HDMI ports to the same
monitor model): pass the ID the message lists, e.g. `--device alsa:hw:CARD=NVidia,DEV=7`.

**"The requested device is temporarily busy" with an `alsa:hw:…` output.** A card opened directly
takes one program at a time, and the sound server is probably using it. Pick the default output,
or the PipeWire/PulseAudio one, instead.

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
