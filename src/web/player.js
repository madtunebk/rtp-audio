// rtp-audio web player: adds a sound button to the page that loads this script, and plays the
// desktop's sound from the WebSocket next to it (Opus through WebCodecs, or raw PCM when the
// browser can't decode Opus). When the server allows it (`--mic`), a microphone button sends
// your microphone to the desktop the same way.
(() => {
  if (window.rtpAudioPlayer) return;
  window.rtpAudioPlayer = true;

  const RATE = 48000;
  const base = new URL(".", document.currentScript.src);
  const wsUrl = new URL("ws", base);
  wsUrl.protocol = wsUrl.protocol === "https:" ? "wss:" : "ws:";
  const micUrl = new URL("mic", base);
  micUrl.protocol = wsUrl.protocol;

  // Plays queued stereo frames, waiting for a little audio first and skipping ahead if a
  // burst (or the sender's clock running fast) makes the queue too long.
  const WORKLET = `
    class RtpAudioPlayer extends AudioWorkletProcessor {
      constructor() {
        super();
        this.queue = []; this.queued = 0; this.offset = 0; this.playing = false;
        this.target = ${RATE} * 0.12; this.max = ${RATE} * 0.4;
        this.port.onmessage = (event) => {
          this.queue.push(event.data); this.queued += event.data[0].length;
          while (this.queued > this.max && this.queue.length > 1) this.queued -= this.queue.shift()[0].length - this.offset, this.offset = 0;
        };
      }
      process(_, [out]) {
        const [left, right] = out;
        if (!this.playing) { if (this.queued < this.target) return true; this.playing = true; }
        for (let i = 0; i < left.length; i++) {
          if (!this.queue.length) { this.playing = false; break; }
          const [l, r] = this.queue[0];
          left[i] = l[this.offset]; right[i] = r[this.offset];
          this.queued--;
          if (++this.offset === l.length) { this.queue.shift(); this.offset = 0; }
        }
        return true;
      }
    }
    registerProcessor("rtp-audio-player", RtpAudioPlayer);`;

  // Hands the browser's microphone over in 20 ms frames (960 samples at 48 kHz).
  const MIC_WORKLET = `
    class RtpAudioMic extends AudioWorkletProcessor {
      constructor() { super(); this.frame = new Float32Array(960); this.filled = 0; }
      process([input]) {
        const samples = input[0];
        if (!samples) return true;
        for (let i = 0; i < samples.length; i++) {
          this.frame[this.filled++] = samples[i];
          if (this.filled === 960) { this.port.postMessage(this.frame, [this.frame.buffer]); this.frame = new Float32Array(960); this.filled = 0; }
        }
        return true;
      }
    }
    registerProcessor("rtp-audio-mic", RtpAudioMic);`;

  // White 25×25 icons in noVNC's style.
  const svg = (body) => "data:image/svg+xml," + encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" width="25" height="25" viewBox="0 0 25 25" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${body}</svg>`);
  const SPEAKER = '<path d="M3.5 10h4l5-4v13l-5-4h-4z" fill="#fff"/>';
  const MIC = '<rect x="9.5" y="3" width="6" height="11" rx="3" fill="#fff"/><path d="M6.5 11.5a6 6 0 0 0 12 0M12.5 17.5v4M9 21.5h7"/>';
  const ICONS = {
    sound: {
      off: svg(SPEAKER + '<path d="M16 9.5l5 5M21 9.5l-5 5"/>'),
      on: svg(SPEAKER + '<path d="M15.5 9a4.5 4.5 0 0 1 0 7M18 6.5a8 8 0 0 1 0 12"/>'),
    },
    mic: {
      off: svg(MIC + '<path d="M4 3.5l17 18" stroke="#e5484d"/>'),
      on: svg(MIC),
    },
    wait: svg('<circle cx="6" cy="12.5" r="1.6" fill="#fff"/><circle cx="12.5" cy="12.5" r="1.6" fill="#fff"/><circle cx="19" cy="12.5" r="1.6" fill="#fff"/>'),
    warn: svg('<path d="M12.5 3.5l10 17h-20z"/><path d="M12.5 10v5M12.5 18v.5"/>'),
  };
  const EMOJI = { sound: { off: "🔇", on: "🔊" }, mic: { off: "🎙️", on: "🎙️" }, wait: "⏳", warn: "⚠️" };

  // A toggle in noVNC's control bar (next to Clipboard and Full screen) when the page has one,
  // otherwise a floating button in the corner. set(state, title) with state off, on, wait, warn.
  const bar = document.querySelector("#noVNC_control_bar .noVNC_scroll");
  let floating = 0;
  function control(kind) {
    let el;
    if (bar) {
      el = document.createElement("input");
      el.type = "image";
      el.className = "noVNC_button";
      const before = document.getElementById("noVNC_fullscreen_button") || document.getElementById("noVNC_settings_button");
      bar.insertBefore(el, before && before.parentNode === bar ? before : null);
    } else {
      el = document.createElement("button");
      el.type = "button";
      // Ignore the page's own button styles (noVNC's would stretch these into ovals).
      el.style.all = "initial";
      Object.assign(el.style, {
        boxSizing: "border-box", padding: "0", margin: "0", lineHeight: "46px", textAlign: "center", fontFamily: "sans-serif",
        position: "fixed", right: "16px", bottom: `${16 + 64 * floating++}px`, zIndex: 2147483647, width: "48px",
        height: "48px", borderRadius: "50%", border: "1px solid rgba(255,255,255,.25)", background: "rgba(20,24,32,.85)",
        color: "#fff", fontSize: "22px", cursor: "pointer", boxShadow: "0 4px 16px rgba(0,0,0,.4)",
      });
      document.body.append(el);
    }
    return {
      el,
      set(state, title) {
        el.title = title;
        el.setAttribute("aria-label", title);
        if (bar) {
          el.src = ICONS[kind][state] || ICONS[state];
          el.alt = title;
          el.classList.toggle("noVNC_selected", state === "on");
        } else {
          el.textContent = EMOJI[kind][state] || EMOJI[state];
          el.style.boxShadow = state === "on" ? "0 0 0 3px #4f9cf9, 0 4px 16px rgba(0,0,0,.4)" : "0 4px 16px rgba(0,0,0,.4)";
        }
      },
    };
  }

  const sound = control("sound");
  const button = sound.el;
  const show = (state, title) => sound.set(state, title);
  show("off", "Desktop sound: off (click to turn on)");

  let session = null;
  // For troubleshooting from the browser console.
  const stats = (window.rtpAudio = { received: 0, decoded: 0, level: 0, codec: null });

  button.addEventListener("click", () => {
    if (session) { session.stop(); session = null; show("off", "Desktop sound: off (click to turn on)"); }
    else session = start();
  });

  // Left and right channels from a decoded AudioData, whatever layout the browser chose: not
  // every browser converts to the format asked for.
  function stereo(data) {
    const n = data.numberOfFrames, channels = data.numberOfChannels;
    const left = new Float32Array(n), right = new Float32Array(n);
    try {
      data.copyTo(left, { planeIndex: 0, format: "f32-planar" });
      data.copyTo(right, { planeIndex: channels > 1 ? 1 : 0, format: "f32-planar" });
      return [left, right];
    } catch {}
    const planar = data.format.endsWith("-planar"), float = data.format.startsWith("f32");
    const scale = float ? 1 : 1 / 32768;
    const read = (plane) => {
      const size = data.allocationSize({ planeIndex: plane });
      const raw = float ? new Float32Array(size / 4) : new Int16Array(size / 2);
      data.copyTo(raw, { planeIndex: plane });
      return raw;
    };
    if (planar) {
      const l = read(0), r = channels > 1 ? read(1) : l;
      for (let i = 0; i < n; i++) { left[i] = l[i] * scale; right[i] = r[i] * scale; }
    } else {
      const all = read(0);
      for (let i = 0; i < n; i++) { left[i] = all[i * channels] * scale; right[i] = all[i * channels + (channels > 1 ? 1 : 0)] * scale; }
    }
    return [left, right];
  }

  function start() {
    let ws, context, node, decoder, retry, stopped = false, timestamp = 0;
    show("wait", "Desktop sound: connecting…");

    const playPcm = (left, right) => {
      let sum = 0;
      for (let i = 0; i < left.length; i++) sum += left[i] * left[i];
      stats.decoded++;
      stats.level = Math.sqrt(sum / left.length);
      node.port.postMessage([left, right], [left.buffer, right.buffer]);
    };

    // Turned off while waiting (permission, loading, a reconnect): a stopped session must not
    // open anything more, or it would keep playing and receiving with no button to stop it.
    async function open() {
      context ||= new AudioContext({ sampleRate: RATE, latencyHint: "interactive" });
      if (!node) {
        await context.audioWorklet.addModule(URL.createObjectURL(new Blob([WORKLET], { type: "text/javascript" })));
        if (stopped) return;
        node = new AudioWorkletNode(context, "rtp-audio-player", { outputChannelCount: [2] });
        node.connect(context.destination);
      }
      await context.resume();
      if (stopped) return;
      const opus = "AudioDecoder" in window &&
        (await AudioDecoder.isConfigSupported({ codec: "opus", sampleRate: RATE, numberOfChannels: 2 }).catch(() => ({}))).supported;
      if (stopped) return;
      if (opus && !decoder) {
        decoder = new AudioDecoder({
          output: (data) => {
            const [left, right] = stereo(data);
            data.close();
            playPcm(left, right);
          },
          error: (err) => console.warn("rtp-audio: decoder", err),
        });
        decoder.configure({ codec: "opus", sampleRate: RATE, numberOfChannels: 2 });
      }
      ws = new WebSocket(wsUrl + (opus ? "" : "?codec=pcm"));
      ws.binaryType = "arraybuffer";
      stats.codec = opus ? "opus" : "pcm";
      ws.onopen = () => show("on", `Desktop sound: on (${opus ? "Opus" : "PCM"}, click to turn off)`);
      ws.onmessage = (event) => {
        if (typeof event.data === "string") return;
        stats.received++;
        if (opus) {
          decoder.decode(new EncodedAudioChunk({ type: "key", timestamp, data: event.data }));
          timestamp += 20000;
        } else {
          const samples = new Int16Array(event.data), n = samples.length / 2;
          const left = new Float32Array(n), right = new Float32Array(n);
          for (let i = 0; i < n; i++) { left[i] = samples[2 * i] / 32768; right[i] = samples[2 * i + 1] / 32768; }
          playPcm(left, right);
        }
      };
      // The sender restarted or the network blinked: try again shortly.
      ws.onclose = () => {
        if (stopped) return;
        show("wait", "Desktop sound: reconnecting…");
        retry = setTimeout(() => open().catch(fail), 2000);
      };
    }

    function fail(err) {
      // A session already turned off doesn't touch the button, or the session that replaced it.
      if (stopped) return;
      console.warn("rtp-audio:", err);
      show("warn", `Desktop sound: ${err.message || err} (click to retry)`);
      stop();
      session = null;
    }

    function stop() {
      stopped = true;
      clearTimeout(retry);
      if (ws) ws.onclose = null, ws.close();
      if (decoder && decoder.state !== "closed") decoder.close();
      if (context && context.state !== "closed") context.close();
    }

    open().catch(fail);
    return { stop };
  }

  // The microphone button, only when the server takes a microphone.
  fetch(new URL("config.json", base), { cache: "no-store" })
    .then((r) => (r.ok ? r.json() : {}))
    .then((config) => { if (config.mic) addMic(); })
    .catch(() => {});

  function addMic() {
    const micControl = control("mic");
    const mic = micControl.el;
    const showMic = (state, title) => micControl.set(state, title);
    showMic("off", "Microphone: off (click to send your microphone to the desktop)");
    stats.mic = { sent: 0 };
    let micSession = null;
    mic.addEventListener("click", () => {
      if (micSession) { micSession.stop(); micSession = null; showMic("off", "Microphone: off (click to send your microphone to the desktop)"); }
      else micSession = startMic();
    });

    function startMic() {
      let ws, context, media, encoder, stopped = false, timestamp = 0;
      showMic("wait", "Microphone: starting…");
      const stop = () => {
        stopped = true;
        if (ws) ws.onclose = null, ws.close();
        if (encoder && encoder.state !== "closed") encoder.close();
        if (media) media.getTracks().forEach((t) => t.stop());
        if (context && context.state !== "closed") context.close();
      };
      const fail = (err) => {
        // A session already turned off doesn't touch the button, or the session that replaced it.
        if (stopped) return;
        console.warn("rtp-audio mic:", err);
        showMic("warn", `Microphone: ${err.message || err} (click to retry)`);
        stop();
        micSession = null;
      };
      (async () => {
        media = await navigator.mediaDevices.getUserMedia({
          audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
        });
        // Turned off while the browser asked for permission: let the microphone go at once.
        if (stopped) return stop();
        context = new AudioContext({ sampleRate: RATE, latencyHint: "interactive" });
        await context.audioWorklet.addModule(URL.createObjectURL(new Blob([MIC_WORKLET], { type: "text/javascript" })));
        if (stopped) return stop();
        const node = new AudioWorkletNode(context, "rtp-audio-mic", { numberOfInputs: 1, numberOfOutputs: 1 });
        // Keep the node running without playing the microphone back.
        const mute = context.createGain();
        mute.gain.value = 0;
        context.createMediaStreamSource(media).connect(node).connect(mute).connect(context.destination);
        const config = { codec: "opus", sampleRate: RATE, numberOfChannels: 1, bitrate: 64000 };
        const opus = "AudioEncoder" in window && (await AudioEncoder.isConfigSupported(config).catch(() => ({}))).supported;
        if (stopped) return stop();
        ws = new WebSocket(micUrl + (opus ? "" : "?codec=pcm"));
        ws.binaryType = "arraybuffer";
        if (opus) {
          encoder = new AudioEncoder({
            output: (chunk) => {
              const data = new Uint8Array(chunk.byteLength);
              chunk.copyTo(data);
              if (ws.readyState === WebSocket.OPEN) { ws.send(data); stats.mic.sent++; }
            },
            error: (err) => console.warn("rtp-audio mic: encoder", err),
          });
          encoder.configure(config);
        }
        node.port.onmessage = ({ data: frame }) => {
          if (stopped || ws.readyState !== WebSocket.OPEN) return;
          if (opus) {
            const audio = new AudioData({ format: "f32", sampleRate: RATE, numberOfFrames: frame.length, numberOfChannels: 1, timestamp, data: frame });
            encoder.encode(audio);
            audio.close();
            timestamp += 20000;
          } else {
            const pcm = new Int16Array(frame.length);
            for (let i = 0; i < frame.length; i++) pcm[i] = Math.max(-32768, Math.min(32767, frame[i] * 32768));
            ws.send(pcm.buffer);
            stats.mic.sent++;
          }
        };
        ws.onopen = () => showMic("on", `Microphone: on (${opus ? "Opus" : "PCM"}, click to turn off)`);
        ws.onclose = (event) => { if (!stopped) fail(event.reason || "the connection closed"); };
      })().catch(fail);
      return { stop };
    }
  }
})();
