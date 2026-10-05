// rtp-audio web player: adds a sound button to the page that loads this script, and plays the
// desktop's sound from the WebSocket next to it (Opus through WebCodecs, or raw PCM when the
// browser can't decode Opus).
(() => {
  if (window.rtpAudioPlayer) return;
  window.rtpAudioPlayer = true;

  const RATE = 48000;
  const base = new URL(".", document.currentScript.src);
  const wsUrl = new URL("ws", base);
  wsUrl.protocol = wsUrl.protocol === "https:" ? "wss:" : "ws:";

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

  const button = document.createElement("button");
  button.type = "button";
  Object.assign(button.style, {
    position: "fixed", right: "16px", bottom: "16px", zIndex: 2147483647, width: "48px", height: "48px",
    borderRadius: "50%", border: "1px solid rgba(255,255,255,.25)", background: "rgba(20,24,32,.85)",
    color: "#fff", fontSize: "22px", cursor: "pointer", boxShadow: "0 4px 16px rgba(0,0,0,.4)",
  });
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
})();
