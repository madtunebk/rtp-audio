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

## Details

- Browsers: current Chrome, Edge, Firefox and Safari. Opus is decoded by the browser
  (WebCodecs); browsers without it get raw 16-bit audio instead, about 1.5 Mbit/s, fine on a LAN.
- Bandwidth with Opus: about 128 kbit/s per listener.
- Latency: about 150 ms plus the network: the player keeps 120 ms of sound in reserve to ride out
  network hiccups, and skips ahead if it falls further behind.
- Troubleshooting: in the browser console, `rtpAudio` shows the frames received and decoded and
  the current level.
- Without NGINX, for a quick test through an SSH tunnel, open `http://127.0.0.1:46080/` directly.
  Never serve on a public address without NGINX: the audio server itself has no login. It warns
  if you try.
