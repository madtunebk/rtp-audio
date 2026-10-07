//! The receiver over UDP: RTP packets (L16, Opus, or encrypted), from one sender at a time, or
//! a multicast group.

use std::error::Error;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use super::status::{Monitor, STATUS_EVERY};
use super::{Buffers, Options, print_volume_and_quit};
use crate::net::secure::{self, Key, ReplayWindow};
use crate::net::{discover, rtp};

/// Turns arriving packets (L16, Opus, or encrypted) into big-endian PCM for the jitter buffer,
/// in two steps: `open` checks a packet is for us (decrypting it with a key), `frames` decodes it.
/// In between, the caller decides whether its sender is the one being played.
pub(super) struct Unpacker {
    key: Option<Key>,
    replay: ReplayWindow,
    decoder: Option<opus::Decoder>,
    channels: usize,
    /// The rate the sound is played at (`--rate`): Opus is decoded straight to it.
    rate: u32,
    last_opus: Option<(u32, u16)>,
    /// Opus packets lost on the way and concealed in the last call to `frames`: the jitter buffer
    /// sees no gap for them, so the receiver counts them as lost itself.
    pub concealed: u16,
    /// Samples per channel in the last Opus packet: concealment fills exactly that much.
    opus_frame: usize,
    warned: Option<String>,
}

/// At most this many lost Opus packets in a row are concealed; beyond, silence.
pub(super) const MAX_CONCEAL: u16 = 5;

/// Payload types played as L16 (16-bit big-endian PCM): rtp-audio's own, the static L16 types
/// (10 stereo, 11 mono), and the dynamic range other tools use for it (PulseAudio, PipeWire,
/// ffmpeg). Other static types (PCMU, PCMA, …) are other codecs, not sound we can play as is.
pub(super) fn is_l16(payload_type: u8) -> bool {
    matches!(payload_type, 10 | 11 | 96..=127) && payload_type != rtp::OPUS && payload_type != secure::PAYLOAD_TYPE
}

impl Unpacker {
    fn new(key: Option<Key>, channels: usize, rate: u32) -> Self {
        let opus_frame = rate as usize / 50;
        Unpacker { key, replay: ReplayWindow::default(), decoder: None, channels, rate, last_opus: None, concealed: 0, opus_frame, warned: None }
    }

    /// A new sender is being played: forget the decoder state of the last one.
    fn restart(&mut self) {
        self.decoder = None;
        self.last_opus = None;
        self.opus_frame = self.rate as usize / 50;
    }

    /// The packet's real payload type and payload if it's for us: decrypted and not replayed
    /// when there's a key, plain otherwise; None (with a warning, once) if not.
    fn open(&mut self, data: &[u8], packet: &rtp::Packet) -> Option<(u8, Vec<u8>)> {
        let (payload_type, payload) = if packet.payload_type == secure::PAYLOAD_TYPE {
            let Some(key) = &self.key else {
                self.warn("Encrypted sound is arriving: start the receiver with --key or --key-file");
                return None;
            };
            match key.open(data) {
                Some((counter, payload_type, payload)) if self.replay.accept(packet.ssrc, counter) => (payload_type, payload),
                Some(_) => return None,
                None => {
                    self.warn("Ignoring packets that don't match the key (wrong key, or not from rtp-audio)");
                    return None;
                }
            }
        } else if self.key.is_some() {
            self.warn("Ignoring unencrypted sound: this receiver only accepts sound encrypted with its key");
            return None;
        } else {
            (packet.payload_type, packet.payload.to_vec())
        };
        if payload_type == rtp::OPUS {
            return Some((payload_type, payload));
        }
        if !is_l16(payload_type) {
            self.warn(&format!("Ignoring RTP payload type {payload_type}: only L16 and Opus can be played"));
            return None;
        }
        if payload.len() % (2 * self.channels) != 0 {
            self.warn(&format!(
                "Ignoring packets whose sound doesn't divide into {}-channel 16-bit frames (try --channels {})",
                self.channels,
                3 - self.channels
            ));
            return None;
        }
        Some((payload_type, payload))
    }

    /// Big-endian PCM frames for an opened packet, oldest first. A lost Opus packet just before
    /// it is concealed rather than left silent; an Opus packet older than the last one decoded,
    /// or the same again, is dropped before it reaches the decoder (it would upset its state).
    fn frames(&mut self, ssrc: u32, sequence: u16, payload_type: u8, payload: Vec<u8>) -> Vec<(u16, Vec<u8>)> {
        self.concealed = 0;
        if payload_type != rtp::OPUS {
            return vec![(sequence, payload)];
        }
        let gap = match self.last_opus {
            Some((last_ssrc, last)) if last_ssrc == ssrc => {
                let step = sequence.wrapping_sub(last);
                if step == 0 || step >= 0x8000 {
                    return Vec::new();
                }
                step - 1
            }
            _ => 0,
        };

        let channels = self.channels;
        if ![8_000, 12_000, 16_000, 24_000, 48_000].contains(&self.rate) {
            self.warn(&format!("Opus sound is arriving: it plays at --rate 8000, 12000, 16000, 24000 or 48000, not {}", self.rate));
            return Vec::new();
        }
        let decoder = match &mut self.decoder {
            Some(decoder) => decoder,
            slot => match opus::Decoder::new(self.rate, if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo }) {
                Ok(decoder) => slot.insert(decoder),
                Err(err) => {
                    eprintln!("cannot decode Opus: {err}");
                    return Vec::new();
                }
            },
        };
        let mut frames = Vec::new();
        let opus_frame = &mut self.opus_frame;
        let mut decode = |decoder: &mut opus::Decoder, sequence: u16, data: &[u8]| -> bool {
            // Without data (a lost packet), Opus conceals as much as the buffer holds.
            let samples = if data.is_empty() { *opus_frame } else { 5760 };
            let mut pcm = vec![0i16; samples * channels];
            match decoder.decode(data, &mut pcm, false) {
                Ok(n) => {
                    if !data.is_empty() {
                        *opus_frame = n;
                    }
                    frames.push((sequence, pcm[..n * channels].iter().flat_map(|s| s.to_be_bytes()).collect()));
                    true
                }
                Err(_) => false,
            }
        };
        if (1..=MAX_CONCEAL).contains(&gap) {
            let last = sequence.wrapping_sub(gap + 1);
            for k in 1..=gap {
                decode(decoder, last.wrapping_add(k), &[]);
            }
            self.concealed = gap;
        }
        // Only a packet that decoded moves the reference on.
        if decode(decoder, sequence, &payload) {
            self.last_opus = Some((ssrc, sequence));
        }
        frames
    }

    fn warn(&mut self, message: &str) {
        if self.warned.as_deref() != Some(message) {
            println!("{message}");
            self.warned = Some(message.to_string());
        }
    }
}

/// RTP packets on a UDP port: plain, Opus or encrypted, from one sender or a multicast group.
pub(super) fn receive_udp(options: &Options, card: &str, jitter: &Buffers, peak: &AtomicU32, monitor: &mut Monitor) -> Result<(), Box<dyn Error>> {
    let socket = bind(options.port, options.group)?;
    println!(
        "Listening on UDP port {} ({} Hz, {} ch, {} ms buffer); {card}",
        options.port, options.rate, options.channels, options.latency_ms
    );
    print_volume_and_quit(options);
    socket.set_read_timeout(Some(STATUS_EVERY))?;
    let mut buf = [0u8; 65536];
    let mut unpacker = Unpacker::new(options.key.clone(), options.channels, options.rate);
    // The sender being played (address and SSRC) and when it was last heard.
    let mut active: Option<(SocketAddr, u32, Instant)> = None;
    if options.key.is_some() {
        println!("Only accepting sound encrypted with the key.");
    }
    if let Some(group) = options.group {
        println!("Also listening to multicast group {group}.");
    }
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, reply_to)) => {
                // On the dual-stack socket an IPv4 sender shows as ::ffff:a.b.c.d: show it as IPv4
                // (but answer it at the address the socket knows).
                let from = SocketAddr::new(reply_to.ip().to_canonical(), reply_to.port());
                let Some(packet) = rtp::parse(&buf[..len]) else {
                    if options.discovery && discover::is_question(&buf[..len]) {
                        let _ = socket.send_to(&discover::answer(options.port, options.key.is_some()), reply_to);
                    }
                    continue;
                };
                let encrypted = packet.payload_type == secure::PAYLOAD_TYPE;
                // With a key, only a packet that opens with it can change who's played.
                let Some((payload_type, payload)) = unpacker.open(&buf[..len], &packet) else {
                    continue;
                };
                // One sender at a time: another is taken over only after the one playing has been
                // quiet for a moment, and then the buffer and the decoder start afresh.
                match active {
                    Some((addr, ssrc, heard)) if (addr, ssrc) != (from, packet.ssrc) => {
                        if heard.elapsed() < SENDER_QUIET {
                            unpacker.warn(&format!("Ignoring sound from {from}: already playing {addr}"));
                            continue;
                        }
                        jitter.restart();
                        unpacker.restart();
                    }
                    _ => {}
                }
                active = Some((from, packet.ssrc, Instant::now()));
                let frames = unpacker.frames(packet.ssrc, packet.sequence, payload_type, payload);
                if unpacker.concealed > 0 {
                    jitter.count_lost(unpacker.concealed);
                }
                if frames.is_empty() {
                    continue;
                }
                monitor.arrived(&from.to_string(), || {
                    let codec = if payload_type == rtp::OPUS { "Opus" } else { "L16" };
                    format!("{codec}, {}", if encrypted { "encrypted" } else { "not encrypted" })
                });
                for (sequence, pcm) in &frames {
                    jitter.push(*sequence, pcm, options.channels);
                }
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => return Err(err.into()),
        }
        monitor.tick(jitter, peak);
    }
}

/// How long the sender being played must be quiet before another one is played instead.
pub(super) const SENDER_QUIET: Duration = Duration::from_secs(1);

/// A UDP socket with a big receive buffer: Windows' default is small enough that a short
/// hiccup in this thread overflows it and loses packets.
pub(super) fn bind(port: u16, group: Option<Ipv4Addr>) -> std::io::Result<UdpSocket> {
    // IPv6 and IPv4 on one socket, where the system allows it; a multicast group (IPv4) needs
    // an IPv4 socket.
    if group.is_none()
        && let Ok(socket) = bind_dual_stack(port)
    {
        return Ok(socket);
    }
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(err) = socket.set_recv_buffer_size(1 << 20) {
        eprintln!("could not enlarge the receive buffer: {err}");
    }
    if group.is_some() {
        // Several receivers on one computer can listen to the same group.
        socket.set_reuse_address(true)?;
    }
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    if let Some(group) = group {
        join_everywhere(&socket, group)?;
    }
    Ok(socket.into())
}

/// A UDP socket taking both IPv6 and IPv4 (as IPv4-mapped addresses) on `port`.
pub(super) fn bind_dual_stack(port: u16) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    if let Err(err) = socket.set_recv_buffer_size(1 << 20) {
        eprintln!("could not enlarge the receive buffer: {err}");
    }
    socket.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    Ok(socket.into())
}

/// Join `group` on every IPv4 network this computer is on: joining with no interface uses only
/// the default route's, so on a computer with several networks (Wi-Fi and cable, a VPN, virtual
/// machines) the group's sound could arrive on another one and never be heard.
pub(super) fn join_everywhere(socket: &Socket, group: Ipv4Addr) -> std::io::Result<()> {
    let mut joined = false;
    for interface in if_addrs::get_if_addrs()? {
        if let std::net::IpAddr::V4(address) = interface.ip()
            && !interface.is_loopback()
            && socket.join_multicast_v4(&group, &address).is_ok()
        {
            joined = true;
        }
    }
    if !joined {
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Unpacker;
    use crate::net::rtp;

    #[test]
    fn conceals_a_lost_opus_packet() {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let tone: Vec<i16> = (0..960 * 2).map(|i| ((i as f32 * 0.03).sin() * 8000.0) as i16).collect();
        let mut out = vec![0u8; 4000];
        let mut packet = |sequence: u16| {
            let len = encoder.encode(&tone, &mut out).unwrap();
            [&rtp::header(sequence, 0, 9, rtp::OPUS)[..], &out[..len]].concat()
        };
        let (first, _lost, third) = (packet(0), packet(1), packet(2));
        let mut unpacker = Unpacker::new(None, 2, 48_000);
        assert_eq!(frames(&mut unpacker, &first), [(0, 3840)]);
        // Packet 1 never arrives: it's concealed (same length) right before packet 2.
        assert_eq!(frames(&mut unpacker, &third), [(1, 3840), (2, 3840)]);
        assert_eq!(unpacker.concealed, 1); // counted as lost by the receiver
    }

    #[test]
    fn opus_decodes_at_the_playing_rate() {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let mut out = vec![0u8; 4000];
        let len = encoder.encode(&vec![0i16; 960 * 2], &mut out).unwrap();
        let packet = [&rtp::header(0, 0, 9, rtp::OPUS)[..], &out[..len]].concat();
        // 20 ms at 16 kHz stereo: 320 frames of 4 bytes.
        assert_eq!(frames(&mut Unpacker::new(None, 2, 16_000), &packet), [(0, 1280)]);
        // A rate Opus can't decode to: nothing, with a warning.
        assert!(frames(&mut Unpacker::new(None, 2, 44_100), &packet).is_empty());
    }

    fn frames(u: &mut Unpacker, data: &[u8]) -> Vec<(u16, usize)> {
        let packet = rtp::parse(data).unwrap();
        let Some((payload_type, payload)) = u.open(data, &packet) else { return Vec::new() };
        u.frames(packet.ssrc, packet.sequence, payload_type, payload).into_iter().map(|(s, pcm)| (s, pcm.len())).collect()
    }

    #[test]
    fn late_and_repeated_opus_packets_skip_the_decoder() {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let tone: Vec<i16> = vec![0; 960 * 2];
        let mut out = vec![0u8; 4000];
        let mut packet = |sequence: u16| {
            let len = encoder.encode(&tone, &mut out).unwrap();
            [&rtp::header(sequence, 0, 9, rtp::OPUS)[..], &out[..len]].concat()
        };
        let (p10, p11, p12, p13) = (packet(10), packet(11), packet(12), packet(13));
        let mut unpacker = Unpacker::new(None, 2, 48_000);
        assert_eq!(frames(&mut unpacker, &p10), [(10, 3840)]);
        assert_eq!(frames(&mut unpacker, &p12), [(11, 3840), (12, 3840)]); // 11 concealed
        assert!(frames(&mut unpacker, &p11).is_empty()); // late: not decoded
        assert!(frames(&mut unpacker, &p12).is_empty()); // the same again
        assert_eq!(frames(&mut unpacker, &p13), [(13, 3840)]); // no new concealment for 12
    }

    #[test]
    fn only_l16_and_opus_are_played() {
        let mut unpacker = Unpacker::new(None, 2, 48_000);
        let with_type = |pt: u8, payload: &[u8]| [&rtp::header(1, 0, 9, pt)[..], payload].concat();
        assert_eq!(frames(&mut unpacker, &with_type(rtp::L16, &[0; 960])), [(1, 960)]);
        assert_eq!(frames(&mut unpacker, &with_type(10, &[0; 960])), [(1, 960)]); // static L16
        assert!(frames(&mut unpacker, &with_type(0, &[0; 960])).is_empty()); // PCMU
        assert!(frames(&mut unpacker, &with_type(8, &[0; 960])).is_empty()); // PCMA
        assert!(frames(&mut unpacker, &with_type(rtp::L16, &[0; 961])).is_empty()); // a broken frame
    }
}
