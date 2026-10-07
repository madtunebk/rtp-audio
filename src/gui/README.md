# The window: `rtp-audio --gui`

A Tauri window with a Svelte page, built into rtp-audio with `--features gui`:

- **Windows, macOS:** the window is part of `rtp-audio(.exe)`; their web views (WebView2,
  WKWebView) come with the system.
- **Linux:** the window is a program of its own, `rtp-audio-gui`, so that rtp-audio itself never
  links WebKitGTK. rtp-audio carries it, writes it once to `~/.cache/rtp-audio/gui-VERSION-HASH/`,
  checks that it can load (with a clear message naming the package to install when WebKitGTK is
  missing) and starts it with `RTP_AUDIO_BIN` set to its own path.

So:

- `rtp-audio find`, `devices`, `send`… run on a server without WebKit;
- the window runs the very rtp-audio that opened it, never another one found in PATH;
- outputs come from `rtp-audio devices --json`, sources from `rtp-audio sources --json`;
- Start runs the command shown under "Command preview" (the receiver with `--json`, whose status
  lines feed the levels, the problems and the spectrum); only `receive` and `send` can be started;
- Stop sends it Ctrl+C, so a sender switches the sound back; closing the window stops it too;
- gain and delay per output become `--device 'ID+180ms@70%'`.

The window still needs the system's web view: WebKitGTK 4.1 on Linux
(`sudo apt install libwebkit2gtk-4.1-0`), WebView2 on Windows (built in), WKWebView on macOS.

## Build

On Ubuntu, building the window needs (see https://v2.tauri.app/start/prerequisites/):

```bash
sudo apt install libwebkit2gtk-4.1-dev libxdo-dev libssl-dev librsvg2-dev
```

Build the page first (Node 20 or newer):

```bash
(cd src/gui/web && npm ci && npm run build)
```

Windows, macOS:

```bash
cargo build --release --features gui
```

Linux: the window program first, then rtp-audio carrying it:

```bash
cargo build --release --manifest-path src/gui/Cargo.toml
RTP_AUDIO_GUI_BIN=$PWD/src/gui/target/release/rtp-audio-gui cargo build --release --features gui
./target/release/rtp-audio --gui
```

Without `gui`, rtp-audio looks for `rtp-audio-gui` next to itself instead. Cross-built from Linux
for Windows (`x86_64-pc-windows-gnu`), the exe also needs `WebView2Loader.dll` beside it; the MSVC
build (the release) links it in.

## The page

In `web/`: `App.svelte` (the page), `Visualizer.svelte` (the spectrum), `style.css`.

- `npm run dev`, then http://127.0.0.1:1420: the page in a browser, as a demo with example
  outputs; nothing runs.
- `npm run check`: Svelte diagnostics.
- `npm test`: the Playwright tests, against the demo, with the installed Google Chrome (set
  `CHROME_BIN` if its path differs).

Outputs are shown as paged rows; the page size follows the window's height. Search and "Active
only" apply across all pages, and "Select results" to all filtered outputs. Settings and the
session log open over the page. Setups (presets) are saved in the window's local storage.
