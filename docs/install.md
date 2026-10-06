# Install on a server: sound and microphone in the browser

This sets up rtp-audio on a Linux server whose desktop you use in the browser through noVNC, as
in the [secure VNC guide](https://gist.github.com/madtunebk/b1909ed9b0bf3826bb6056286b5e13bc):
NGINX with HTTPS, and OauthRS for the login. Afterwards the noVNC page has two more buttons in
its control bar: 🔊 to hear the desktop, and 🎙️ to send your microphone to it. Both go through
the same HTTPS connection and login, so this works on a VPS too, with no extra ports.

No OauthRS, only a VNC password? Then let NGINX ask for a password instead: see
[without OauthRS](#without-oauthrs-a-password-in-nginx) in step 3.

Run everything as the desktop's user, logged in over SSH as that user (not through `su` or
`sudo -u`). The examples use the user `nobus`, with user ID 1000 (`id -u` shows yours).

## 1. Let the desktop find the sound server

The user's sound server (PulseAudio, or PipeWire's PulseAudio server) runs as a user service.
Keep it running even when nobody is logged in:

```bash
sudo loginctl enable-linger nobus
```

The VNC desktop is started by a system service, which doesn't know where that sound server is.
Open it:

```bash
sudo nano /etc/systemd/system/vncserver.service
```

and add this line below `Environment=HOME=/home/nobus`:

```ini
Environment=XDG_RUNTIME_DIR=/run/user/1000
```

```bash
sudo systemctl daemon-reload
sudo systemctl restart vncserver.service
```

Restarting closes any open desktop connection.

## 2. Install rtp-audio

```bash
wget https://github.com/madtunebk/rtp-audio/releases/download/v0.3.1/rtp-audio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz
tar -xzf rtp-audio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz
sudo install -m 755 rtp-audio-v0.3.1-x86_64-unknown-linux-gnu/rtp-audio /usr/local/bin/rtp-audio
```

On an ARM server (e.g. a Raspberry Pi), use the `aarch64-unknown-linux-gnu` archive. To upgrade
later, run the same three lines with the new version, then `rtp-audio service restart`.

## 3. Add it to NGINX

```bash
sudo nano /etc/nginx/sites-available/default
```

Inside `location / { … }`, after `try_files $uri $uri/ =404;`, add the line that puts the buttons
on the noVNC page:

```nginx
        # rtp-audio: adds the sound and microphone buttons to the noVNC page.
        sub_filter '</body>' '<script src="/audio/player.js"></script></body>';
        sub_filter_once on;
```

Above `location = /websockify {`, add the audio location, protected by the same login:

```nginx
    # rtp-audio: the desktop's sound and your microphone, for logged-in browsers only.
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

```bash
sudo nginx -t
sudo systemctl reload nginx
```

### Without OauthRS: a password in NGINX

If your noVNC is behind NGINX with only a VNC password, add a login in NGINX first. The VNC
password does not protect the sound: only the VNC server checks it, and the audio server has no
login of its own. One NGINX password for the whole site protects the desktop, its sound and the
microphone, and the browser asks for it once.

```bash
sudo apt install -y apache2-utils
sudo htpasswd -c /etc/nginx/desktop.htpasswd nobus
```

In your site's `server { … }` block (the HTTPS one), add:

```nginx
    # A password for the whole site: the desktop, its sound and the microphone.
    auth_basic "Remote desktop";
    auth_basic_user_file /etc/nginx/desktop.htpasswd;
```

Add the `sub_filter` lines to `location /` as above, and this audio location, without the
`auth_request` line, since the password above already covers it:

```nginx
    # rtp-audio: the desktop's sound and your microphone.
    location /audio/ {
        proxy_pass http://127.0.0.1:46080/;
        proxy_http_version 1.1;

        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;

        # Keep the password out of the audio server.
        proxy_set_header Authorization "";

        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        proxy_buffering off;
    }
```

`$connection_upgrade` needs a `map` in the `http` context; if your configuration doesn't have
one yet, add it at the top of the site file, outside `server { … }`:

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}
```

Test and reload as above. Without the password, `/audio/player.js` now answers `401`:

```bash
curl -k -s -o /dev/null -w '%{http_code}\n' https://localhost/audio/player.js
```

## 4. Run it as a service

```bash
rtp-audio service install --web 46080 --mic
rtp-audio service status
```

No `sudo`: it's your user's service. It starts now and at every boot, adds the "RTP Audio" output
and the "RTP Audio Microphone" input to the desktop, and serves them on `127.0.0.1:46080` only.
Leave out `--mic` if you only want to listen.

## 5. Use it

Open the desktop in your browser and log in. In noVNC's control bar on the left:

- **Speaker:** the desktop's sound on or off.
- **Microphone:** sends your microphone to the desktop; allow it when the browser asks.

Browsers only start sound after a click, so nothing plays until you press the speaker. Use
headphones with the microphone, so the desktop doesn't hear itself.

## If something doesn't work

- **`Failed to connect to user scope bus`**, or rtp-audio says your user's service manager is not
  running: run `sudo loginctl enable-linger nobus`, then log in over SSH as `nobus` directly.
- **No buttons in noVNC:** check the `sub_filter` lines are inside `location /`, and reload the
  page. Without a login, `/audio/` answers `401` and no buttons appear, which is intended.
- **A button shows a warning sign:** hover it for the reason. `rtp-audio service status` shows the
  server's side.
- **No sound, but the speaker is on:** check something plays on the desktop, and in the browser
  console, `rtpAudio` should show `received` and `decoded` growing and a `level` above 0.
- **Never** open port 46080 in a firewall: the audio server has no login of its own and only
  listens on localhost; NGINX is the way in.

More detail: [sound in the browser](web.md), [running as a service](setup.md#run-it-automatically).
