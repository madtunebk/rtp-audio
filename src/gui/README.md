# The window: `rtp-audio --gui`

A Tauri window with a Svelte page, built into rtp-audio with the `gui` feature. It runs the very
rtp-audio that opened it, so the two are always the same version:

- outputs from `rtp-audio devices --json`, sources from `rtp-audio sources --json`;
- Start runs the command shown under "Command preview" (the receiver with `--json`, whose status
  lines feed the levels, the problems and the spectrum); only `receive` and `send` can be started;
- Stop sends it Ctrl+C, so a sender switches the sound back; closing the window stops it too;
- gain and delay per output become `--device 'ID+180ms@70%'`.

## Build

On Ubuntu, Tauri needs WebKitGTK 4.1 (see https://v2.tauri.app/start/prerequisites/):

```bash
sudo apt install libwebkit2gtk-4.1-dev libxdo-dev libssl-dev librsvg2-dev
```

Build the page first (Node 20 or newer), then rtp-audio with the window:

```bash
cd src/gui/web
npm ci
npm run build
cd ../../..
cargo build --release --features gui
./target/release/rtp-audio --gui
```

Without `--features gui`, rtp-audio builds as before and needs neither Node nor WebKitGTK.

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
