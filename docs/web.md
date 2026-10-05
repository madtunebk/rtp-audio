# Sound in the browser

`rtp-audio send --web` serves the desktop's sound to web browsers: Opus over a WebSocket, with
a small player that adds a 🔊 button to the page. Behind NGINX it travels inside the same HTTPS
connection and behind the same login as the noVNC desktop, so it is safe to use over the
internet, unlike the plain UDP stream. Nothing needs to be installed on the computer you watch
from.

These steps follow the [secure VNC guide](https://gist.github.com/madtunebk/b1909ed9b0bf3826bb6056286b5e13bc):
NGINX with HTTPS, and OauthRS protecting `location /` and `/websockify`.

## 1. Start serving on the server

On the server, as the desktop's user:

```bash
./rtp-audio send --web 46080
```

It listens on `127.0.0.1:46080` only (a bare port means localhost), adds the "RTP Audio" output
and makes it the default, exactly like `rtp-audio send HOST:PORT`. You can also do both at once,
e.g. `rtp-audio send 192.168.1.20:46000 --web 46080`, or serve a single source with `--source`.

Opus is only encoded while at least one browser listens, about 1–2% of one CPU core, and once
for all listeners.

To have it start with the desktop, and keep running after you log out of SSH, install it as a
service instead:

```bash
./rtp-audio service install --web 46080
```

`rtp-audio service status`, `stop`, `start` and `uninstall` manage it; see
[running as a service](setup.md#run-it-automatically).

## 2. Add it to NGINX

```bash
sudo nano /etc/nginx/sites-available/default
```

Inside `location / { … }`, after `try_files`, add the line that puts the sound button on the noVNC
page:

```nginx
        # rtp-audio: adds the sound button to the noVNC page.
        sub_filter '</body>' '<script src="/audio/player.js"></script></body>';
        sub_filter_once on;
```

Above `location = /websockify`, add the audio location, protected by the same login:

```nginx
    # rtp-audio: the desktop's sound, for logged-in browsers only.
    location /audio/ {
        auth_request /auth;

        proxy_pass http://127.0.0.1:46080/;
        proxy_http_version 1.1;

        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;

        # Keep credentials out of the audio server.
        proxy_set_header Cookie "";
        proxy_set_header Authorization "";

        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        proxy_buffering off;
    }
```

Check and reload:

```bash
sudo nginx -t
sudo systemctl reload nginx
```

## 3. Listen

Open the desktop in your browser as usual and log in. A 🔇 button sits in the bottom-right
corner: click it to turn the sound on (🔊). Browsers only allow sound after a click, so it never
starts by itself.

If the sender restarts, the player reconnects by itself (⏳). Without a login, `/audio/` answers
`401`, so nobody can listen without an account.

## Your microphone, the other way

Add `--mic` to also send your microphone to the remote desktop, e.g. for a call in a browser or
app running there:

```bash
./rtp-audio send --web 46080 --mic
```

The page then shows a second button, 🎙️. Click it and allow the microphone: the desktop gets a
new input, **"RTP Audio Microphone"**, made the default while rtp-audio runs, so apps pick it up
by themselves. Your browser cleans the sound up first (echo cancellation, noise suppression),
compresses it to Opus (about 64 kbit/s) and sends it through the same HTTPS connection and
login. One browser at a time can use it; the next one is told the microphone is busy.

Use headphones: the browser can't cancel the desktop's own sound coming out of your speakers.
When rtp-audio stops, the default input is switched back, and the microphone removed.

## Details

- Browsers: current Chrome, Edge, Firefox and Safari. Opus is decoded by the browser
  (WebCodecs); browsers without it get raw 16-bit audio instead, about 1.5 Mbit/s, fine on a LAN.
- Bandwidth with Opus: about 128 kbit/s per listener.
- Latency: about 150 ms plus the network: the player keeps 120 ms of sound in reserve to ride out
  network hiccups, and skips ahead if it falls further behind.
- Troubleshooting: in the browser console, `rtpAudio` shows the frames received and decoded, the
  current level, and with the microphone on, `rtpAudio.mic.sent`.
- The microphone needs HTTPS (browsers only allow it on secure pages), which NGINX provides.
- Without NGINX, for a quick test through an SSH tunnel, open `http://127.0.0.1:46080/` directly.
  Never serve on a public address without NGINX: the audio server itself has no login. It warns
  if you try.
