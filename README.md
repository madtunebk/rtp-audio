# rtp-audio

Send your Linux desktop's sound over the network and play it on one or several outputs at once.
Use the desktop window or the command line — both are in the same `rtp-audio` executable.

**[Download the latest release](https://github.com/madtunebk/rtp-audio/releases/latest)** ·
[Setup guide](docs/setup.md) · [Troubleshooting](docs/troubleshooting.md)

## Open the window

Download and extract the archive for your platform, then run:

```bash
rtp-audio --gui
```

On Windows, use `rtp-audio.exe --gui`. On Linux, use `./rtp-audio --gui` if the executable
is in the current directory rather than your PATH.

- **Receive:** choose UDP or WebSocket, select your outputs, and press **Start**.
- **Send on Linux:** choose a destination and transport, then press **Start**.
- Adjust each output's gain and delay, enable synchronization, and save setups as presets.
- Watch the live spectrum, output levels and stream status in the same window.

The window manages its audio process: no separate backend terminal is needed, and closing
the window stops its session.

## What it does

- **Several outputs, one stream:** a separate buffer, gain and delay for each output.
- **Automatic synchronization:** `--sync` aligns outputs using their reported latency.
  Extra delay can be set manually when a speaker or display does not report its full latency.
- **PCM or Opus:** uncompressed 16-bit PCM or Opus at **16–510 kbit/s**, with 128 kbit/s as the
  default Opus bitrate.
- **UDP or WebSocket/TCP:** direct network playback or the web stream, including through an SSH tunnel.
- **Browser sound and microphone:** browser playback and optional microphone return to Linux apps,
  with noVNC and HTTPS reverse-proxy integration.
- **LAN discovery and multicast:** find receivers and send to several computers.
- **Optional UDP encryption:** ChaCha20-Poly1305 with replay protection using a shared key.
- **Linux integration:** desktop or selected-source capture, routing restoration when stopped,
  and a systemd user service for the sender.

## Platforms

Linux and Windows are the main target platforms. macOS receives and has the window, but hasn't been
tested on a real Mac yet.

| Platform | Receive | Capture and send desktop sound | Desktop GUI |
| --- | --- | --- | --- |
| Linux | Yes | Yes, through PulseAudio or PipeWire's PulseAudio service | Yes |
| Windows | Yes | Not yet | Yes |
| macOS | Yes; not yet tested on a real Mac | No | Yes; not yet tested on a real Mac |

The release contains one application executable per platform. Linux CLI mode does **not** need
WebKitGTK; the Linux GUI needs WebKitGTK 4.1. Windows GUI uses the WebView2 Runtime, which must be
available on the machine. Node.js is needed only when building from source.
See [runtime requirements and building](docs/building.md).

## Quick start from the terminal

Start the receiver on the computer where you want to hear the sound:

```bash
rtp-audio receive
```

Then, on the Linux computer playing the sound, use the receiver's IP address:

```bash
rtp-audio send 192.168.1.20:46000 --opus --bitrate 256
```

Allow inbound **UDP port 46000** in the receiver's firewall if prompted. Press **Ctrl+C** to stop;
the Linux sender restores the previous sound routing.

### Play on several outputs

List the devices, then replace the example IDs with names or IDs from that list:

```bash
rtp-audio devices
rtp-audio receive --device "OUTPUT_ID_1, OUTPUT_ID_2" --sync --latency 120
```

Use one comma-separated `--device` argument. Double quotes work in Linux shells and Windows
CMD/PowerShell. An output can have its own delay and gain, for example `OUTPUT_ID_1+80ms@70%`.
The GUI provides these controls without typing device IDs.

Bluetooth may need manual delay compensation, especially on Windows. Synchronization depends
on the latency each device or sound server reports; see [setup](docs/setup.md).

### Browser and WebSocket playback

On Linux:

```bash
rtp-audio send --web 46080 --bitrate 256
```

This listens on `127.0.0.1:46080` only. Browsers hear it through a noVNC page that loads its
player, behind NGINX with HTTPS and a login ([install on a server](docs/install.md)); there is
no page of its own. A native receiver can use it directly, e.g. through an SSH tunnel:

```bash
rtp-audio receive ws://127.0.0.1:46080
```

Here `127.0.0.1` means the receiver's own machine. For a remote sender, use a tunnel or configure
an accessible server address. See [browser setup](docs/web.md).

UDP is unencrypted unless a key is configured. HTTPS protects the browser connection, not a
separate UDP stream. See [encrypted UDP setup](docs/setup.md#smaller-and-encrypted).

## Docs

- [Setup](docs/setup.md): step by step, sending one source, all options
- [Install on a server](docs/install.md): sound and microphone in the noVNC page, step by step
- [Sound in the browser](docs/web.md): Opus over HTTPS, behind the login
- [Troubleshooting](docs/troubleshooting.md)
- [Building](docs/building.md): build, runtime requirements and limitations
- [How it works](docs/how-it-works.md)
