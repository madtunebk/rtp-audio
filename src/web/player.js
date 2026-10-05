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

  const makeButton = (right) => {
    const b = document.createElement("button");
    b.type = "button";
    Object.assign(b.style, {
      position: "fixed", right, bottom: "16px", zIndex: 2147483647, width: "48px", height: "48px",
      borderRadius: "50%", border: "1px solid rgba(255,255,255,.25)", background: "rgba(20,24,32,.85)",
      color: "#fff", fontSize: "22px", cursor: "pointer", boxShadow: "0 4px 16px rgba(0,0,0,.4)",
    });
    return b;
  };
  const button = makeButton("16px");
  const show = (icon, title) => { button.textContent = icon; button.title = title; button.setAttribute("aria-label", title); };
  show("🔇", "Desktop sound: off (click to turn on)");
  document.body.append(button);

  let session = null;
  // For troubleshooting from the browser console.
  const stats = (window.rtpAudio = { received: 0, decoded: 0, level: 0, codec: null });

  button.addEventListener("click", () => {
    if (session) { session.stop(); session = null; show("🔇", "Desktop sound: off (click to turn on)"); }
    else session = start();
  });

  function start() {
    let ws, context, node, decoder, retry, stopped = false, timestamp = 0;
    show("⏳", "Desktop sound: connecting…");

    const playPcm = (left, right) => {
      let sum = 0;
      for (let i = 0; i < left.length; i++) sum += left[i] * left[i];
      stats.decoded++;
      stats.level = Math.sqrt(sum / left.length);
      node.port.postMessage([left, right], [left.buffer, right.buffer]);
    };

    async function open() {
      context ||= new AudioContext({ sampleRate: RATE, latencyHint: "interactive" });
      if (!node) {
        await context.audioWorklet.addModule(URL.createObjectURL(new Blob([WORKLET], { type: "text/javascript" })));
        node = new AudioWorkletNode(context, "rtp-audio-player", { outputChannelCount: [2] });
        node.connect(context.destination);
      }
      await context.resume();
      const opus = "AudioDecoder" in window &&
        (await AudioDecoder.isConfigSupported({ codec: "opus", sampleRate: RATE, numberOfChannels: 2 }).catch(() => ({}))).supported;
      if (opus && !decoder) {
        decoder = new AudioDecoder({
          output: (data) => {
            const left = new Float32Array(data.numberOfFrames), right = new Float32Array(data.numberOfFrames);
            data.copyTo(left, { planeIndex: 0, format: "f32-planar" });
            data.copyTo(right, { planeIndex: 1, format: "f32-planar" });
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
      ws.onopen = () => show("🔊", `Desktop sound: on (${opus ? "Opus" : "PCM"}, click to turn off)`);
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
        show("⏳", "Desktop sound: reconnecting…");
        retry = setTimeout(() => open().catch(fail), 2000);
      };
    }

    function fail(err) {
      console.warn("rtp-audio:", err);
      show("⚠️", `Desktop sound: ${err.message || err} (click to retry)`);
      stop();
      session = null;
    }

    function stop() {
      stopped = true;
      clearTimeout(retry);
      if (ws) ws.onclose = null, ws.close();
      if (decoder && decoder.state !== "closed") decoder.close();
      if (context) context.close();
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
    const mic = makeButton("72px");
    const showMic = (icon, title, on) => {
      mic.textContent = icon; mic.title = title; mic.setAttribute("aria-label", title);
      mic.style.boxShadow = on ? "0 0 0 3px #e5484d, 0 4px 16px rgba(0,0,0,.4)" : "0 4px 16px rgba(0,0,0,.4)";
    };
    showMic("🎙️", "Microphone: off (click to send your microphone to the desktop)", false);
    document.body.append(mic);
    stats.mic = { sent: 0 };
    let micSession = null;
    mic.addEventListener("click", () => {
      if (micSession) { micSession.stop(); micSession = null; showMic("🎙️", "Microphone: off (click to send your microphone to the desktop)", false); }
      else micSession = startMic();
    });

    function startMic() {
      let ws, context, media, encoder, stopped = false, timestamp = 0;
      showMic("⏳", "Microphone: starting…", false);
      const stop = () => {
        stopped = true;
        if (ws) ws.onclose = null, ws.close();
        if (encoder && encoder.state !== "closed") encoder.close();
        if (media) media.getTracks().forEach((t) => t.stop());
        if (context) context.close();
      };
      const fail = (err) => {
        console.warn("rtp-audio mic:", err);
        showMic("⚠️", `Microphone: ${err.message || err} (click to retry)`, false);
        stop();
        micSession = null;
      };
      (async () => {
        media = await navigator.mediaDevices.getUserMedia({
          audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
        });
        context = new AudioContext({ sampleRate: RATE, latencyHint: "interactive" });
        await context.audioWorklet.addModule(URL.createObjectURL(new Blob([MIC_WORKLET], { type: "text/javascript" })));
        const node = new AudioWorkletNode(context, "rtp-audio-mic", { numberOfInputs: 1, numberOfOutputs: 1 });
        // Keep the node running without playing the microphone back.
        const mute = context.createGain();
        mute.gain.value = 0;
        context.createMediaStreamSource(media).connect(node).connect(mute).connect(context.destination);
        const config = { codec: "opus", sampleRate: RATE, numberOfChannels: 1, bitrate: 64000 };
        const opus = "AudioEncoder" in window && (await AudioEncoder.isConfigSupported(config).catch(() => ({}))).supported;
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
            encoder.encode(new AudioData({ format: "f32", sampleRate: RATE, numberOfFrames: frame.length, numberOfChannels: 1, timestamp, data: frame }));
            timestamp += 20000;
          } else {
            const pcm = new Int16Array(frame.length);
            for (let i = 0; i < frame.length; i++) pcm[i] = Math.max(-32768, Math.min(32767, frame[i] * 32768));
            ws.send(pcm.buffer);
            stats.mic.sent++;
          }
        };
        ws.onopen = () => showMic("🎙️", `Microphone: on (${opus ? "Opus" : "PCM"}, click to turn off)`, true);
        ws.onclose = (event) => { if (!stopped) fail(event.reason || "the connection closed"); };
      })().catch(fail);
      return { stop };
    }
  }
})();
