//! Sending: cut raw big-endian 16-bit PCM into RTP packets (L16, or Opus), optionally encrypt
//! them, and send them over UDP.

use std::error::Error;
use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

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
    channels: usize,
    frames_per_packet: u32,
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
        let packet_ms = if encoder.is_some() { OPUS_PACKET_MS } else { PCM_PACKET_MS };
        let frames_per_packet = rate * packet_ms / 1000;
        let ssrc = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()) ^ std::process::id();
        Ok(Self {
            socket,
            ssrc,
            sequence: 0,
            timestamp: 0,
            channels,
            frames_per_packet,
            encoder,
            key: encoding.key,
            counter: 0,
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
        self.timestamp = self.timestamp.wrapping_add(self.frames_per_packet);
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
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => sender.push(&buf[..n])?,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
}

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
        // 1.5 packets in odd-sized pieces: exactly one packet goes out.
        let audio: Vec<u8> = (0..1440).map(|i| i as u8).collect();
        for chunk in audio.chunks(100) {
            sender.push(chunk).unwrap();
        }
        let mut buf = [0u8; 2048];
        let len = receiver.recv(&mut buf).unwrap();
        let packet = rtp::parse(&buf[..len]).unwrap();
        assert_eq!((packet.sequence, packet.payload_type), (0, rtp::L16));
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
        for counter in 1..=2 {
            let len = receiver.recv(&mut buf).unwrap();
            assert_eq!(buf[1] & 0x7f, secure::PAYLOAD_TYPE);
            let (got, payload_type, payload) = key.open(&buf[..len]).unwrap();
            assert_eq!((got, payload_type), (counter, rtp::OPUS));
            let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
            let mut out = vec![0i16; 960 * 2];
            assert_eq!(decoder.decode(&payload, &mut out, false).unwrap(), 960);
        }
    }
}
