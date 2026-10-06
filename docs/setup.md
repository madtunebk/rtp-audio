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

## Smaller and encrypted

By default the sound travels as plain 16-bit audio, about 1.5 Mbit/s, which any RTP L16
receiver can play. Between two rtp-audio programs you can do better:

```bash
./rtp-audio keygen > ~/.rtp-audio.key && chmod 600 ~/.rtp-audio.key   # once; copy the file to the other computer
./rtp-audio send 192.168.1.20:46000 --opus --key-file ~/.rtp-audio.key   # sender
rtp-audio --key-file rtp-audio.key                                       # receiver
```

- `--opus` compresses the sound to about 150 kbit/s with no audible difference: fine over Wi-Fi,
  a VPN or a phone hotspot. A lost packet is filled in by Opus instead of becoming silence.
- `--key-file` (or `--key KEY`) encrypts and authenticates every packet with ChaCha20-Poly1305.
  Nobody without the key can listen, change the sound or replay it. A receiver with a key
  ignores anything not encrypted with it, and tells you why if the keys don't match.
- Prefer `--key-file`: a `--key` on the command line is visible to other users in `ps`.

The receiver recognises plain, Opus and encrypted sound by itself; only `--key` must be given.

## Finding receivers, and several listeners

On a local network you don't need to look up addresses:

```bash
./rtp-audio find                      # lists the receivers that are running: name, address
./rtp-audio send auto                 # sends to the receiver `find` sees, if there's only one
```

Receivers answer `find` unless started with `--no-discovery`. Discovery uses a broadcast, so it
stays on the local network: over a VPN, give the address.

To send to several computers, list them, or use a multicast group that any number of receivers
can join (the group stays on the local network):

```bash
./rtp-audio send 192.168.1.20:46000 192.168.1.30:46000     # two receivers
./rtp-audio send 239.255.46.1:46000 --opus                  # a multicast group…
rtp-audio --group 239.255.46.1                              # …on every receiver that wants it
```

## Over TCP: through an SSH tunnel, or when UDP struggles

UDP is the default and the quickest: nothing waits for a lost packet. But it needs a direct path
from the sender to the receiver, and on a busy link (e.g. alongside an RDP session that takes the
bandwidth) packets get lost and the sound breaks up. The receiver can instead play the sender's
web stream (`--web`), which is Opus over TCP:

```bash
./rtp-audio send --web 46080                     # on the desktop: listens on localhost only
ssh -L 46080:localhost:46080 USER@DESKTOP        # on your computer: a tunnel to it, left open
./rtp-audio ws://localhost:46080                 # on your computer, in another terminal
```

- Nothing is lost on the way, and it goes wherever SSH goes: no open ports, encrypted by SSH, and
  the desktop needs nothing listening on its network.
- When the network is slow, TCP waits instead of losing; the receiver then skips ahead so the
  delay doesn't keep growing. Give it a bigger buffer if the sound breaks up: `--latency 150`.
- It reconnects by itself when the sender restarts.
- The same sender serves browsers and TCP receivers at once, and with a service
  (`rtp-audio service install --web 46080`) it's always there.
- `ws://` only for now: behind NGINX's HTTPS, use a tunnel too.

| | UDP (default) | TCP (`ws://`) |
|---|---|---|
| A lost packet | gone; Opus hides the gap | sent again |
| A busy network | the sound breaks up | it waits, then skips ahead |
| Through SSH or a proxy | no | yes |
| Several receivers, multicast, `find` | yes | each connects on its own |
| Encryption | `--key-file` | the tunnel's |

## Run it automatically

Instead of starting the sender by hand in an SSH session, install it as a user service. It
then starts with the desktop, restarts if something goes wrong, and switches the sound back when
stopped:

```bash
./rtp-audio service install 192.168.1.20:46000     # the same options as `rtp-audio send`
./rtp-audio service status                          # is it running? recent messages
./rtp-audio service stop                            # also: start, restart
./rtp-audio service uninstall                       # stop it and remove it
```

The service runs the `rtp-audio` file you installed it from, so keep that file where it is. To
start it at boot without anyone logged in, also run `sudo loginctl enable-linger $USER`; the
install command tells you when that's needed. No other part of rtp-audio runs external programs:
`service` uses `systemctl --user`.

## All options

```
rtp-audio [receive] [--port 46000] [--latency 60] [--device NAME_OR_ID] [--volume 100]
                    [--key-file FILE] [--group 239.255.46.1] [--no-discovery]
rtp-audio [receive] ws://HOST:PORT [--latency 60] [--device NAME_OR_ID] [--volume 100]   # over TCP
rtp-audio find [--port 46000]                                     # receivers on this network
rtp-audio devices                                                 # outputs for --device
rtp-audio keygen                                                  # a key for --key / --key-file
rtp-audio sources
rtp-audio send HOST:PORT [HOST:PORT…] [--source NAME_OR_ID] [--opus] [--key-file FILE]   # or `auto`
rtp-audio send HOST:PORT --stdin [--rate 48000] [--channels 2]   # raw s16be PCM from stdin
rtp-audio send --web 46080 [--mic] [HOST:PORT]                   # sound (and microphone) in the browser, see web.md
rtp-audio service install SEND_OPTIONS | uninstall | status | start | stop | restart
```

`--latency` is the receiver's buffer in milliseconds, `--volume` is in percent (0–400). In a
terminal, the receiver shows a live status line: packets per second, how full its buffer is,
problems in the last few seconds, and a level meter of what arrives. Something went wrong? See
[troubleshooting](troubleshooting.md).
