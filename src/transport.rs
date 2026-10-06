//! Sending: cut raw big-endian 16-bit PCM into RTP packets (L16, or Opus), optionally encrypt
//! them, and send them over UDP.

use std::error::Error;
use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::rtp;
use crate::secure::Key;

/// L16: 5 ms of audio per packet, small enough for low latency, ~1 KB so it never fragments.
const PCM_PACKET_MS: u32 = 5;
/// Opus: 20 ms per packet, its usual frame size.
const OPUS_PACKET_MS: u32 = 20;
const OPUS_BITRATE: i32 = 128_000;

/// Somewhere captured sound goes: raw big-endian 16-bit PCM, 48 kHz stereo.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub trait AudioSink {
    fn push(&mut self, pcm: &[u8]) -> std::io::Result<()>;
}

impl AudioSink for RtpSender {
    fn push(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        RtpSender::push(self, pcm)
    }
}

/// How the sound travels: plain L16 (works with any RTP L16 receiver) or Opus, and whether
/// it's encrypted.
#[derive(Clone, Default)]
pub struct Encoding {
    pub opus: bool,
    pub key: Option<Key>,
}

pub struct RtpSender {
    socket: UdpSocket,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    /// How far the RTP timestamp moves per packet: samples at the rate sent, except for Opus,
    /// whose RTP clock always runs at 48 kHz.
    timestamp_step: u32,
    channels: usize,
    encoder: Option<opus::Encoder>,
    key: Option<Key>,
    counter: u64,
    /// PCM waiting to fill the next packet.
    pending: Vec<u8>,
    packet_bytes: usize,
    opus_out: Vec<u8>,
}

impl RtpSender {
    /// A UDP socket connected to `destination`. Fails early if there is no route to it.
    pub fn connect(destination: SocketAddr, rate: u32, channels: usize, encoding: Encoding) -> Result<Self, Box<dyn Error>> {
        let local: SocketAddr = if destination.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
        let socket = UdpSocket::bind(local)?;
        if destination.ip().is_multicast() {
            // Stay on the local network.
            socket.set_multicast_ttl_v4(1)?;
        }
        socket.connect(destination)?;
        let encoder = if encoding.opus {
            if ![8_000, 12_000, 16_000, 24_000, 48_000].contains(&rate) {
                return Err(format!("Opus needs a rate of 8000, 12000, 16000, 24000 or 48000 Hz, not {rate}").into());
            }
            let layout = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            let mut encoder = opus::Encoder::new(rate, layout, opus::Application::Audio)?;
            encoder.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))?;
            Some(encoder)
        } else {
            None
        };
        if !crate::cli::RATES.contains(&rate) {
            return Err(format!("the rate must be between {} and {} Hz, not {rate}", crate::cli::RATES.start(), crate::cli::RATES.end()).into());
        }
        let packet_ms = if encoder.is_some() { OPUS_PACKET_MS } else { PCM_PACKET_MS };
        let frames_per_packet = rate * packet_ms / 1000;
        let timestamp_step = if encoder.is_some() { 48_000 * packet_ms / 1000 } else { frames_per_packet };
        // A random SSRC, and random starting sequence, timestamp and counter (RFC 3550 asks for the
        // first three). With a key, SSRC + counter is the nonce: random starts keep two sessions
        // using the same key (restarts, other computers) from ever reusing one.
        let mut random = [0u8; 18];
        getrandom::fill(&mut random).map_err(|err| format!("cannot get random bytes: {err}"))?;
        let ssrc = u32::from_be_bytes(random[0..4].try_into().unwrap());
        let sequence = u16::from_be_bytes(random[4..6].try_into().unwrap());
        let timestamp = u32::from_be_bytes(random[6..10].try_into().unwrap());
        // 62 bits: far from wrapping, however long the session runs.
        let counter = u64::from_be_bytes(random[10..18].try_into().unwrap()) >> 2;
        Ok(Self {
            socket,
            ssrc,
            sequence,
            timestamp,
            timestamp_step,
            channels,
            encoder,
            key: encoding.key,
            counter,
            pending: Vec::new(),
            packet_bytes: frames_per_packet as usize * channels * 2,
            opus_out: vec![0; 4000],
        })
    }

    /// Queue raw big-endian 16-bit PCM, sending every packet that fills up.
    pub fn push(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let n = (self.packet_bytes - self.pending.len()).min(data.len());
            self.pending.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.pending.len() == self.packet_bytes {
                self.send_packet()?;
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// Send what's left at the end of the input: L16 as a shorter packet of whole frames, Opus
    /// padded with silence to a full frame (it only takes fixed sizes).
    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.encoder.is_some() {
            if self.pending.is_empty() {
                return Ok(());
            }
            self.pending.resize(self.packet_bytes, 0);
        } else {
            let whole = self.pending.len() / (self.channels * 2) * self.channels * 2;
            self.pending.truncate(whole);
            if self.pending.is_empty() {
                return Ok(());
            }
        }
        self.send_packet()?;
        self.pending.clear();
        Ok(())
    }

    fn send_packet(&mut self) -> std::io::Result<()> {
        let (payload_type, payload): (u8, &[u8]) = match &mut self.encoder {
            None => (rtp::L16, &self.pending),
            Some(encoder) => {
                let samples: Vec<i16> = self.pending.chunks_exact(2).map(|b| i16::from_be_bytes([b[0], b[1]])).collect();
                let len = encoder
                    .encode(&samples, &mut self.opus_out)
                    .map_err(|err| std::io::Error::other(format!("Opus encoding failed: {err}")))?;
                (rtp::OPUS, &self.opus_out[..len])
            }
        };
        let header = rtp::header(self.sequence, self.timestamp, self.ssrc, payload_type);
        let packet = match &self.key {
            Some(key) => {
                self.counter += 1;
                key.seal(&header, self.counter, payload_type, payload)
            }
            None => [&header[..], payload].concat(),
        };
        match self.socket.send(&packet) {
            // The receiver may not be running yet; keep sending.
            Err(err) if err.kind() != ErrorKind::ConnectionRefused => return Err(err),
            _ => {}
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.timestamp_step);
        Ok(())
    }

    pub fn describe(&self) -> String {
        let codec = if self.encoder.is_some() { "Opus" } else { "L16" };
        let channels = if self.channels == 1 { "mono" } else { "stereo" };
        let secure = if self.key.is_some() { ", encrypted" } else { ", not encrypted" };
        format!("{codec} {channels}{secure}")
    }
}

/// Read raw big-endian 16-bit PCM from stdin (e.g. `ffmpeg ... -f s16be -`) and send it as RTP
/// to `destination` until stdin ends.
pub fn send_stdin(destination: SocketAddr, rate: u32, channels: usize, encoding: Encoding) -> Result<(), Box<dyn Error>> {
    let mut sender = RtpSender::connect(destination, rate, channels, encoding)?;
    eprintln!("Sending {rate} Hz, {} from stdin to {destination}", sender.describe());
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    // Paced to real time: a file (or anything faster than live) is sent as it would play, not
    // all at once. Measured from the start, so waits never add up to a drift; live input, which
    // arrives in time anyway, is never held back by more than the lead.
    let started = Instant::now();
    let bytes_per_second = f64::from(rate) * channels as f64 * 2.0;
    let mut sent = 0u64;
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => {
                sender.finish()?;
                return Ok(());
            }
            Ok(n) => {
                sender.push(&buf[..n])?;
                sent += n as u64;
                let due = Duration::from_secs_f64(sent as f64 / bytes_per_second);
                if let Some(ahead) = due.checked_sub(started.elapsed() + STDIN_LEAD) {
                    std::thread::sleep(ahead);
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// How far ahead of real time `--stdin` may send: enough for the receiver's buffer to fill.
const STDIN_LEAD: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::{Encoding, RtpSender};
    use crate::rtp;
    use crate::secure::{self, Key};
    use std::net::UdpSocket;
    use std::time::Duration;

    fn receiver() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        socket
    }

    #[test]
    fn packetizes_into_5_ms_packets() {
        let receiver = receiver();
        let mut sender = RtpSender::connect(receiver.local_addr().unwrap(), 48_000, 2, Encoding::default()).unwrap();
        let first_sequence = sender.sequence;
        // 1.5 packets in odd-sized pieces: exactly one packet goes out.
        let audio: Vec<u8> = (0..1440).map(|i| i as u8).collect();
        for chunk in audio.chunks(100) {
            sender.push(chunk).unwrap();
        }
        let mut buf = [0u8; 2048];
        let len = receiver.recv(&mut buf).unwrap();
        let packet = rtp::parse(&buf[..len]).unwrap();
        assert_eq!((packet.sequence, packet.payload_type), (first_sequence, rtp::L16));
        assert_eq!(packet.payload, &audio[..960]);
        receiver.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        assert!(receiver.recv(&mut buf).is_err());
    }

    #[test]
    fn sends_encrypted_opus_in_20_ms_packets() {
        let receiver = receiver();
        let key = Key::parse(&secure::generate().unwrap()).unwrap();
        let encoding = Encoding { opus: true, key: Some(key.clone()) };
        let mut sender = RtpSender::connect(receiver.local_addr().unwrap(), 48_000, 2, encoding).unwrap();
        sender.push(&vec![0u8; 960 * 4 * 2]).unwrap(); // 40 ms: two packets
        let mut buf = [0u8; 2048];
        let mut counters = Vec::new();
        for _ in 0..2 {
            let len = receiver.recv(&mut buf).unwrap();
            assert_eq!(buf[1] & 0x7f, secure::PAYLOAD_TYPE);
            let (got, payload_type, payload) = key.open(&buf[..len]).unwrap();
            assert_eq!(payload_type, rtp::OPUS);
            counters.push(got);
            let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
            let mut out = vec![0i16; 960 * 2];
            assert_eq!(decoder.decode(&payload, &mut out, false).unwrap(), 960);
        }
        // Consecutive within the session, from a random start.
        assert_eq!(counters[1], counters[0] + 1);
    }

    #[test]
    fn opus_timestamps_run_at_48_khz_whatever_the_rate() {
        let receiver = receiver();
        let encoding = Encoding { opus: true, key: None };
        let mut sender = RtpSender::connect(receiver.local_addr().unwrap(), 8_000, 1, encoding).unwrap();
        sender.push(&vec![0u8; 160 * 2 * 2]).unwrap(); // two 20 ms packets at 8 kHz mono
        let mut buf = [0u8; 2048];
        let mut stamps = Vec::new();
        for _ in 0..2 {
            receiver.recv(&mut buf).unwrap();
            stamps.push(u32::from_be_bytes(buf[4..8].try_into().unwrap()));
        }
        assert_eq!(stamps[1].wrapping_sub(stamps[0]), 960);
    }

    #[test]
    fn rates_outside_the_audio_range_are_refused() {
        let to = receiver().local_addr().unwrap();
        assert!(RtpSender::connect(to, 1, 2, Encoding::default()).is_err());
        assert!(RtpSender::connect(to, 400_000, 2, Encoding::default()).is_err());
    }

    #[test]
    fn each_session_starts_somewhere_else() {
        let receiver = receiver();
        let to = receiver.local_addr().unwrap();
        let a = RtpSender::connect(to, 48_000, 2, Encoding::default()).unwrap();
        let b = RtpSender::connect(to, 48_000, 2, Encoding::default()).unwrap();
        assert_ne!((a.ssrc, a.counter), (b.ssrc, b.counter));
        assert!(a.counter < 1 << 62 && b.counter < 1 << 62);
    }
}
