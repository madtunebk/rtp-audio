# The window: `rtp-audio --gui`

A Tauri window with a Svelte page, built into rtp-audio by default (feature `gui`):

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

Part of the normal build: `cargo build --release` (see [docs/building.md](../../docs/building.md)).
This crate's build script builds the page with npm (`npm ci` the first time) before embedding it;
on Linux, rtp-audio's build script builds this crate as a program, in `target/gui-program/`, and
carries it. `RTP_AUDIO_GUI_BIN` makes it carry an already built one instead.

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
