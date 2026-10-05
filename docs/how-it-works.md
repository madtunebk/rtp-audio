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

It also plays standard RTP L16 streams (PulseAudio `module-rtp-send`, PipeWire `module-rtp-sink`,
`ffmpeg -f rtp -acodec pcm_s16be`) at 48 kHz stereo.
