# rtp-audio

Hear a remote Linux desktop's sound on your own computer, over the network. One small program:

- `rtp-audio send` on the **Linux** computer whose sound you want (e.g. the VNC server): captures
  its sound and sends it as RTP over UDP.
- `rtp-audio` on the computer you sit at (**Windows** or **Linux**): listens on UDP port 46000 and
  plays what arrives. On Windows it's a single `.exe`, no DLLs or installer.
- Or **in the browser**: `rtp-audio send --web 46080` serves the sound as Opus behind NGINX, with a
  🔊 button on the noVNC page. Nothing to install where you listen, and it travels inside HTTPS
  and behind the login. See [sound in the browser](docs/web.md).

> The UDP stream is **unencrypted by default**: noVNC and NGINX's HTTPS don't cover it. Use it on
> a LAN or through a private VPN, encrypt it with `--key-file` (see [setup](docs/setup.md#smaller-and-encrypted)),
> or use the browser mode instead.

## Quick start

```bash
rtp-audio                               # on your computer (Windows: rtp-audio.exe)
rtp-audio send 192.168.1.20:46000       # on the Linux desktop: your computer's IP; Ctrl+C stops
```

Download from [releases](https://github.com/madtunebk/rtp-audio/releases).

## Docs

- [Setup](docs/setup.md): step by step, sending one source, all options
- [Install on a server](docs/install.md): sound and microphone in the noVNC page, step by step
- [Sound in the browser](docs/web.md): Opus over HTTPS, behind the login
- [Troubleshooting](docs/troubleshooting.md)
- [Building](docs/building.md): build, runtime requirements and limitations
- [How it works](docs/how-it-works.md)
