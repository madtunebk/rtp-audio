//! Sending: cut raw big-endian 16-bit PCM into RTP packets and send them over UDP.

use std::error::Error;
use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::rtp;

/// 5 ms of audio per packet: small enough for low latency, ~1 KB so it never fragments.
const PACKET_MS: u32 = 5;

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

pub struct RtpSender {
    socket: UdpSocket,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    frames_per_packet: u32,
    /// The packet being filled: RTP header space, then `filled` bytes of payload.
    packet: Vec<u8>,
    filled: usize,
}

impl RtpSender {
    /// A UDP socket connected to `destination`. Fails early if there is no route to it.
    pub fn connect(destination: SocketAddr, rate: u32, channels: usize) -> std::io::Result<Self> {
        let local: SocketAddr = if destination.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
        let socket = UdpSocket::bind(local)?;
        socket.connect(destination)?;
        let frames_per_packet = rate * PACKET_MS / 1000;
        let payload_len = frames_per_packet as usize * channels * 2;
        let ssrc = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()) ^ std::process::id();
        Ok(Self {
            socket,
            ssrc,
            sequence: 0,
            timestamp: 0,
            frames_per_packet,
            packet: vec![0; rtp::HEADER_LEN + payload_len],
            filled: 0,
        })
    }

    /// Queue raw big-endian 16-bit PCM, sending every packet that fills up.
    pub fn push(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        let payload_len = self.packet.len() - rtp::HEADER_LEN;
        while !data.is_empty() {
            let n = (payload_len - self.filled).min(data.len());
            let start = rtp::HEADER_LEN + self.filled;
            self.packet[start..start + n].copy_from_slice(&data[..n]);
            self.filled += n;
            data = &data[n..];
            if self.filled == payload_len {
                self.send_packet()?;
                self.filled = 0;
            }
        }
        Ok(())
    }

    fn send_packet(&mut self) -> std::io::Result<()> {
        self.packet[..rtp::HEADER_LEN].copy_from_slice(&rtp::header(self.sequence, self.timestamp, self.ssrc));
        match self.socket.send(&self.packet) {
            // The receiver may not be running yet; keep sending.
            Err(err) if err.kind() != ErrorKind::ConnectionRefused => return Err(err),
            _ => {}
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.frames_per_packet);
        Ok(())
    }
}

/// Read raw big-endian 16-bit PCM from stdin (e.g. `ffmpeg ... -f s16be -`) and send it as RTP
/// to `destination` until stdin ends.
pub fn send_stdin(destination: SocketAddr, rate: u32, channels: usize) -> Result<(), Box<dyn Error>> {
    let mut sender = RtpSender::connect(destination, rate, channels)?;
    eprintln!("Sending {rate} Hz, {channels} ch from stdin to {destination}");
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
    use super::RtpSender;
    use crate::rtp;
    use std::net::UdpSocket;
    use std::time::Duration;

    #[test]
    fn packetizes_into_5_ms_packets() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut sender = RtpSender::connect(receiver.local_addr().unwrap(), 48_000, 2).unwrap();
        // 1.5 packets in odd-sized pieces: exactly one packet goes out.
        let audio: Vec<u8> = (0..1440).map(|i| i as u8).collect();
        for chunk in audio.chunks(100) {
            sender.push(chunk).unwrap();
        }
        let mut buf = [0u8; 2048];
        let len = receiver.recv(&mut buf).unwrap();
        let packet = rtp::parse(&buf[..len]).unwrap();
        assert_eq!(packet.sequence, 0);
        assert_eq!(packet.payload, &audio[..960]);
        receiver.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        assert!(receiver.recv(&mut buf).is_err());
    }
}
