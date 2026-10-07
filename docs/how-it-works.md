# How it works

**Sender.** It connects to the user's sound server through libpulse and asks for 48 kHz stereo
16-bit audio, either from the "RTP Audio" output's monitor (automatic mode) or from the chosen
source. It sends it in 5 ms RTP packets (L16, payload type 97) over UDP.

In automatic mode it records each change it makes to the sound settings in
`$XDG_RUNTIME_DIR/rtp-audio/` before making it, and marks its output with a random ID. Stopping,
a failed start and the next start after a crash all undo from that record, so it only ever
removes its own output and leaves changes you made meanwhile alone. A lock held by the kernel
keeps a second sender from starting, and it can't go stale after a crash.

**Receiver.** Packets go into a jitter buffer (60 ms by default) that absorbs network hiccups;
lost packets become silence. The sound card pulls from it through a small resampler, which also
plays up to 0.5% faster or slower to keep the buffer level, since the sender's and the sound
card's clocks never run at exactly the same speed.

Given a `ws://` URL instead, the receiver connects to the sender's web stream (`--web`): a
WebSocket over TCP, first a short header (codec, rate, channels), then one Opus frame per 20 ms.
It decodes them into the same jitter buffer. TCP never loses a frame, but on a slow link they
arrive late and in bursts; the buffer then drops the oldest sound so the delay stays near the
buffer size. The sender encodes once for every browser and TCP receiver, and gives each a short
queue, so one slow listener misses frames without holding back the others.

The receiver plays one sender at a time, by address and SSRC; another is taken over after a
second of quiet, with the buffer and the decoder started afresh. It plays L16 (payload types
10, 11 and 96–127) and Opus, and ignores other codecs. With several outputs, each has its own
jitter buffer and resampler fed with the same sound; cards opened directly (ALSA `hw:`) are asked
for 40 ms periods (an 80 ms hardware ring with CPAL), independently of `--latency`.
Native PulseAudio outputs keep 20 ms periods. A preallocated SPSC queue connects the network
producer to each callback; the callback owns its jitter buffer and never waits for the network
or the visualizer. Full input queues drop new packets and count a skip; the callback also trims
old buffered sound on bursts. Very short jitter targets are raised to cover a hardware callback
plus interpolation. This can increase effective latency; output synchronization must include
the hardware latency too.

FFT runs on a separate worker with a two-packet queue. Status/JSON output runs on another
worker with a two-snapshot queue. Slow analysis or GUI/log consumers drop visual updates
without holding up reception or playback. All hardware xruns, including startup ones, are
counted; error text is emitted outside the audio callback.

**Encryption.** Each sender session starts with a random SSRC, sequence, timestamp and 62-bit
packet counter, and SSRC + counter is the ChaCha20-Poly1305 nonce, so sessions sharing a key
practically never reuse a nonce. The receiver keeps a replay window for each of the last 32
senders. There is no handshake, so a receiver started later can't tell a recording of an earlier
session from a live sender: change the key to make old recordings useless.

It also plays standard RTP L16 streams (PulseAudio `module-rtp-send`, PipeWire `module-rtp-sink`,
`ffmpeg -f rtp -acodec pcm_s16be`) at 48 kHz stereo.
