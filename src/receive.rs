use std::error::Error;
use std::io::{ErrorKind, IsTerminal, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use socket2::{Domain, Protocol, Socket, Type};
use tungstenite::Message;

use crate::discover;
use crate::jitter::{Jitter, Player, Stats};
use crate::rtp;
use crate::secure::{self, Key, ReplayWindow};

pub struct Options {
    pub port: u16,
    pub latency_ms: u32,
    pub rate: u32,
    pub channels: usize,
    /// The --device value: an output (name, part of a name, or ID), or several separated by
    /// commas, which play the same sound at once; the default output if empty.
    pub devices: Vec<String>,
    /// 1.0 is unchanged.
    pub volume: f32,
    /// Only accept packets encrypted with this key.
    pub key: Option<Key>,
    /// Also listen to this multicast group.
    pub group: Option<Ipv4Addr>,
    /// Answer `rtp-audio find`.
    pub discovery: bool,
    /// Play the sender's WebSocket stream (ws://…) instead of listening for UDP.
    pub url: Option<String>,
}

/// Turns arriving packets (L16, Opus, or encrypted) into big-endian PCM for the jitter buffer.
struct Unpacker {
    key: Option<Key>,
    replay: ReplayWindow,
    decoder: Option<opus::Decoder>,
    channels: usize,
    last_opus: Option<(u32, u16)>,
    /// Samples per channel in the last Opus packet: concealment fills exactly that much.
    opus_frame: usize,
    warned: Option<&'static str>,
}

/// At most this many lost Opus packets in a row are concealed; beyond, silence.
const MAX_CONCEAL: u16 = 5;

impl Unpacker {
    /// The packet's sound as (sequence, big-endian PCM) frames, oldest first; empty if it's
    /// not for us. A lost Opus packet just before it is concealed rather than left silent.
    fn unpack(&mut self, data: &[u8], packet: &rtp::Packet) -> Vec<(u16, Vec<u8>)> {
        let (payload_type, payload) = if packet.payload_type == secure::PAYLOAD_TYPE {
            let Some(key) = &self.key else {
                self.warn("Encrypted sound is arriving: start the receiver with --key or --key-file");
                return Vec::new();
            };
            match key.open(data) {
                Some((counter, payload_type, payload)) if self.replay.accept(packet.ssrc, counter) => (payload_type, payload),
                Some(_) => return Vec::new(),
                None => {
                    self.warn("Ignoring packets that don't match the key (wrong key, or not from rtp-audio)");
                    return Vec::new();
                }
            }
        } else if self.key.is_some() {
            self.warn("Ignoring unencrypted sound: this receiver only accepts sound encrypted with its key");
            return Vec::new();
        } else {
            (packet.payload_type, packet.payload.to_vec())
        };
        if payload_type != rtp::OPUS {
            return vec![(packet.sequence, payload)];
        }

        let channels = self.channels;
        let decoder = match &mut self.decoder {
            Some(decoder) => decoder,
            slot => match opus::Decoder::new(48_000, if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo }) {
                Ok(decoder) => slot.insert(decoder),
                Err(err) => {
                    eprintln!("cannot decode Opus: {err}");
                    return Vec::new();
                }
            },
        };
        let mut frames = Vec::new();
        let opus_frame = &mut self.opus_frame;
        let mut decode = |decoder: &mut opus::Decoder, sequence: u16, data: &[u8]| {
            // Without data (a lost packet), Opus conceals as much as the buffer holds.
            let samples = if data.is_empty() { *opus_frame } else { 5760 };
            let mut pcm = vec![0i16; samples * channels];
            if let Ok(n) = decoder.decode(data, &mut pcm, false) {
                if !data.is_empty() {
                    *opus_frame = n;
                }
                frames.push((sequence, pcm[..n * channels].iter().flat_map(|s| s.to_be_bytes()).collect()));
            }
        };
        if let Some((ssrc, last)) = self.last_opus
            && ssrc == packet.ssrc
        {
            let gap = packet.sequence.wrapping_sub(last).wrapping_sub(1);
            if (1..=MAX_CONCEAL).contains(&gap) {
                for k in 1..=gap {
                    decode(decoder, last.wrapping_add(k), &[]);
                }
            }
        }
        decode(decoder, packet.sequence, &payload);
        self.last_opus = Some((packet.ssrc, packet.sequence));
        frames
    }

    fn warn(&mut self, message: &'static str) {
        if self.warned != Some(message) {
            println!("{message}");
            self.warned = Some(message);
        }
    }
}

fn device_name(device: &cpal::Device) -> String {
    device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "(unnamed)".into())
}

fn device_id(device: &cpal::Device) -> String {
    device.id().map(|id| id.to_string()).unwrap_or_default()
}

struct Outlet {
    name: String,
    id: String,
    device: cpal::Device,
}

/// The outputs, each name once (ALSA lists every card under several names: hw, plughw, …).
fn outputs() -> Result<Vec<Outlet>, Box<dyn Error>> {
    let mut seen = std::collections::HashSet::new();
    Ok(cpal::default_host()
        .output_devices()?
        .map(|device| Outlet { name: device_name(&device), id: device_id(&device), device })
        .filter(|outlet| seen.insert(outlet.name.clone()))
        .collect())
}

/// `rtp-audio devices`: the sound outputs this computer can play on.
pub fn list_devices() -> Result<(), Box<dyn Error>> {
    let default = cpal::default_host().default_output_device().map(|d| device_id(&d));
    let outputs = outputs()?;
    let width = outputs.iter().map(|o| o.name.chars().count()).max().unwrap_or(0);
    println!("Sound outputs (play on one with: rtp-audio --device NAME_OR_ID):");
    for Outlet { name, id, .. } in &outputs {
        let mark = if Some(id) == default.as_ref() { "*" } else { " " };
        println!(" {mark} {name:width$}   {id}");
    }
    println!("\n* = the default output");
    Ok(())
}

/// The output with this ID or name, else the only one whose name contains it; or the default.
fn pick_device(wanted: Option<&str>) -> Result<cpal::Device, Box<dyn Error>> {
    let Some(wanted) = wanted else {
        return cpal::default_host().default_output_device().ok_or_else(|| "no sound output device".into());
    };
    let mut outputs = outputs()?;
    let lower = wanted.to_lowercase();
    if let Some(i) = outputs.iter().position(|o| o.id == wanted || o.name.to_lowercase() == lower) {
        return Ok(outputs.swap_remove(i).device);
    }
    let matches: Vec<usize> = (0..outputs.len()).filter(|&i| outputs[i].name.to_lowercase().contains(&lower)).collect();
    match matches.as_slice() {
        [i] => Ok(outputs.swap_remove(*i).device),
        [] => Err(format!("no sound output matches '{wanted}'; see `rtp-audio devices`").into()),
        _ => Err(format!(
            "'{wanted}' matches several outputs ({}); use more of the name, or its ID",
            matches.iter().map(|&i| outputs[i].name.as_str()).collect::<Vec<_>>().join(", ")
        )
        .into()),
    }
}

/// The outputs to play on: the default one, or each in the --device list. Device names and IDs
/// can contain commas themselves ("HD-Audio Generic, ALC897 Analog", alsa:hw:CARD=Generic,DEV=0),
/// so the list is read left to right, taking each time the longest run of comma-separated parts
/// that is exactly an output's name or ID, else one part as (part of) a name.
fn pick_devices(wanted: &[String]) -> Result<Vec<cpal::Device>, Box<dyn Error>> {
    if wanted.is_empty() {
        return Ok(vec![pick_device(None)?]);
    }
    let outputs = outputs()?;
    let exact = |text: &str| outputs.iter().position(|o| o.id == text || o.name.eq_ignore_ascii_case(text));
    let mut devices = Vec::new();
    for value in wanted {
        let parts: Vec<&str> = value.split(',').collect();
        let mut i = 0;
        while i < parts.len() {
            let longest = (i + 1..=parts.len()).rev().find_map(|j| exact(parts[i..j].join(",").trim()).map(|k| (j, k)));
            match longest {
                Some((j, k)) => {
                    devices.push(outputs[k].device.clone());
                    i = j;
                }
                None => {
                    let part = parts[i].trim();
                    if !part.is_empty() {
                        devices.push(pick_device(Some(part))?);
                    }
                    i += 1;
                }
            }
        }
    }
    // The same output named twice would play the sound twice, slightly apart.
    let mut seen = std::collections::HashSet::new();
    devices.retain(|device| seen.insert(device_id(device)));
    if devices.is_empty() {
        return Err("--device names no output; see `rtp-audio devices`".into());
    }
    Ok(devices)
}

/// The jitter buffers of all the outputs: every frame goes into each.
struct Buffers(Vec<Arc<Mutex<Jitter>>>);

impl Buffers {
    fn push(&self, sequence: u16, pcm: &[u8], channels: usize) {
        for jitter in &self.0 {
            jitter.lock().unwrap().push(sequence, pcm, channels);
        }
    }

    /// For the status line: the worst of each problem across the outputs, and the emptiest buffer.
    fn state(&self) -> (Stats, usize) {
        let mut stats = Stats::default();
        let mut buffered = usize::MAX;
        for jitter in &self.0 {
            let jitter = jitter.lock().unwrap();
            let s = jitter.stats;
            stats.lost = stats.lost.max(s.lost);
            stats.late = stats.late.max(s.late);
            stats.underruns = stats.underruns.max(s.underruns);
            stats.trimmed = stats.trimmed.max(s.trimmed);
            buffered = buffered.min(jitter.buffered());
        }
        (stats, if buffered == usize::MAX { 0 } else { buffered })
    }
}

/// Receive RTP audio on a UDP port, or the sender's WebSocket stream, and play it on a sound
/// card until killed.
pub fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let target = (options.rate * options.latency_ms / 1000).max(1) as usize;
    // Loudest sample played since the status line last looked, as f32 bits.
    let peak = Arc::new(AtomicU32::new(0));
    // One buffer per output: each sound card runs on its own clock, so each keeps its own level.
    let (mut buffers, mut streams, mut cards) = (Vec::new(), Vec::new(), Vec::new());
    for device in pick_devices(&options.devices)? {
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let jitter = Arc::new(Mutex::new(Jitter::new(target)));
        let out = Output { jitter: Arc::clone(&jitter), peak: Arc::clone(&peak), volume: options.volume, input_rate: options.rate };
        let stream = match format {
            SampleFormat::F32 => play::<f32>(&device, &config, out),
            SampleFormat::I16 => play::<i16>(&device, &config, out),
            SampleFormat::U16 => play::<u16>(&device, &config, out),
            SampleFormat::I32 => play::<i32>(&device, &config, out),
            SampleFormat::F64 => play::<f64>(&device, &config, out),
            other => return Err(format!("{}: sample format {other} is not supported", device_name(&device)).into()),
        }?;
        stream.play()?;
        cards.push(format!("{} ({} Hz, {} ch, {format})", device_name(&device), config.sample_rate, config.channels));
        buffers.push(jitter);
        streams.push(stream);
    }
    let card = if cards.len() == 1 { format!("sound card: {}", cards[0]) } else { format!("{} sound cards: {}", cards.len(), cards.join("; ")) };
    let jitter = Buffers(buffers);
    let mut monitor = Monitor::new(options.rate);
    match &options.url {
        Some(url) => receive_websocket(url, &options, &card, &jitter, &peak, &mut monitor),
        None => receive_udp(&options, &card, &jitter, &peak, &mut monitor),
    }
}

fn print_volume_and_quit(options: &Options) {
    if options.volume != 1.0 {
        println!("Volume: {:.0}%", options.volume * 100.0);
    }
    println!("Ctrl+C to quit.");
}

/// RTP packets on a UDP port: plain, Opus or encrypted, from one sender or a multicast group.
fn receive_udp(options: &Options, card: &str, jitter: &Buffers, peak: &AtomicU32, monitor: &mut Monitor) -> Result<(), Box<dyn Error>> {
    let socket = bind(options.port, options.group)?;
    println!(
        "Listening on UDP port {} ({} Hz, {} ch, {} ms buffer); {card}",
        options.port, options.rate, options.channels, options.latency_ms
    );
    print_volume_and_quit(options);
    socket.set_read_timeout(Some(STATUS_EVERY))?;
    let mut buf = [0u8; 65536];
    let mut unpacker = Unpacker { key: options.key.clone(), replay: ReplayWindow::default(), decoder: None, channels: options.channels, last_opus: None, opus_frame: 960, warned: None };
    if options.key.is_some() {
        println!("Only accepting sound encrypted with the key.");
    }
    if let Some(group) = options.group {
        println!("Also listening to multicast group {group}.");
    }
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let Some(packet) = rtp::parse(&buf[..len]) else {
                    if options.discovery && discover::is_question(&buf[..len]) {
                        let _ = socket.send_to(&discover::answer(options.port, options.key.is_some()), from);
                    }
                    continue;
                };
                let encrypted = packet.payload_type == secure::PAYLOAD_TYPE;
                let frames = unpacker.unpack(&buf[..len], &packet);
                if frames.is_empty() {
                    continue;
                }
                monitor.arrived(&from.to_string(), || {
                    let codec = if unpacker.last_opus.is_some_and(|(ssrc, _)| ssrc == packet.ssrc) { "Opus" } else { "L16" };
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

/// How long to wait before connecting again after the sender's stream ends or can't be reached.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

/// The sender's WebSocket stream (`rtp-audio send --web`): Opus over TCP, so nothing is lost
/// and it goes through an SSH tunnel or a proxy; it reconnects by itself when the sender restarts.
fn receive_websocket(url: &str, options: &Options, card: &str, jitter: &Buffers, peak: &AtomicU32, monitor: &mut Monitor) -> Result<(), Box<dyn Error>> {
    let target = WsTarget::parse(url)?;
    println!("Receiving from {url} over TCP ({} ms buffer); {card}", options.latency_ms);
    print_volume_and_quit(options);
    let mut last_problem = String::new();
    // Frame numbers for the jitter buffer, carried across reconnections: starting again from 0
    // would look like old, late sound to it.
    let mut sequence: u16 = 0;
    loop {
        let problem = match websocket_session(&target, options, jitter, peak, monitor, &mut sequence) {
            Ok(()) => "the sender closed the connection".to_string(),
            Err(err) => err.to_string(),
        };
        if problem != last_problem {
            if monitor.live {
                print!("\r{:width$}\r", "", width = STATUS_WIDTH);
            }
            println!("{url}: {problem}; trying again every {} s", RECONNECT_EVERY.as_secs());
            last_problem = problem;
        }
        monitor.forget_sender();
        let retry = Instant::now() + RECONNECT_EVERY;
        while Instant::now() < retry {
            std::thread::sleep(STATUS_EVERY);
            monitor.tick(jitter, peak);
        }
    }
}

/// One connection to the sender's stream, until it ends.
fn websocket_session(target: &WsTarget, options: &Options, jitter: &Buffers, peak: &AtomicU32, monitor: &mut Monitor, sequence: &mut u16) -> Result<(), Box<dyn Error>> {
    let stream = TcpStream::connect((target.host.as_str(), target.port))?;
    // Frames are small and every 20 ms: send them at once rather than gathering them.
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let peer = stream.peer_addr()?.to_string();
    let (mut socket, _) = tungstenite::client(target.request.as_str(), stream)
        .map_err(|err| format!("no rtp-audio stream there ({err})"))?;

    // First a text message saying what follows: {"codec":"opus","sampleRate":48000,"channels":2,…}.
    let header = loop {
        match socket.read()? {
            Message::Text(text) => break text.to_string(),
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    };
    if !header.contains(r#""codec":"opus""#) {
        return Err(format!("unexpected stream from the sender: {header}").into());
    }
    let channels = header
        .split(r#""channels":"#)
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(2);
    if channels != options.channels {
        return Err(format!("the sender sends {channels} channel(s); start the receiver with --channels {channels}").into());
    }
    let mut decoder = opus::Decoder::new(48_000, if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo })?;
    socket.get_mut().set_read_timeout(Some(STATUS_EVERY))?;

    let mut pcm = vec![0i16; 5760 * channels];
    loop {
        match socket.read() {
            Ok(Message::Binary(data)) => {
                let samples = decoder.decode(&data, &mut pcm, false)?;
                let frame: Vec<u8> = pcm[..samples * channels].iter().flat_map(|s| s.to_be_bytes()).collect();
                jitter.push(*sequence, &frame, channels);
                *sequence = sequence.wrapping_add(1);
                monitor.arrived(&peer, || "Opus over TCP".to_string());
            }
            // A sender stopped with Ctrl+C just drops the connection: that's an ordinary end too.
            Ok(Message::Close(_))
            | Err(tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake)) => return Ok(()),
            Err(tungstenite::Error::Io(err)) if err.kind() == ErrorKind::ConnectionReset => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(err)) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => return Err(err.into()),
        }
        monitor.tick(jitter, peak);
    }
}

/// Where a `ws://HOST[:PORT][/PATH]` URL points. The sender serves its stream at /ws; a URL that
/// ends in / (e.g. behind a proxy at /audio/) gets ws added.
struct WsTarget {
    host: String,
    port: u16,
    request: String,
}

impl WsTarget {
    fn parse(url: &str) -> Result<Self, String> {
        if url.starts_with("wss://") {
            return Err("wss:// isn't supported yet: use ws:// through an SSH tunnel (ssh -L 46080:localhost:46080 SERVER)".into());
        }
        let rest = url.strip_prefix("ws://").ok_or_else(|| format!("'{url}' is not a ws:// URL"))?;
        let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().map_err(|_| format!("bad port in '{url}'"))?),
            None => (authority, 80),
        };
        if host.is_empty() {
            return Err(format!("no host in '{url}'"));
        }
        let path = if path.ends_with('/') { format!("{path}ws") } else { path.to_string() };
        Ok(WsTarget { host: host.to_string(), port, request: format!("ws://{authority}{path}") })
    }
}

/// The live status line at a terminal, or occasional reports in a log, for either source.
struct Monitor {
    live: bool,
    rate: u32,
    sender: Option<String>,
    packets: u32,
    last_status: Instant,
    shown: Stats,
    last_report: Instant,
    reported: Stats,
    last_packet: Instant,
    silent_reported: bool,
}

impl Monitor {
    fn new(rate: u32) -> Self {
        let now = Instant::now();
        Monitor {
            // At a terminal, one status line updates in place; in a log, only events and problems.
            live: std::io::stdout().is_terminal(),
            rate,
            sender: None,
            packets: 0,
            last_status: now,
            shown: Stats::default(),
            last_report: now,
            reported: Stats::default(),
            last_packet: now,
            silent_reported: true,
        }
    }

    /// Sound arrived from `from`; when that's a new sender, says so with `describe()`.
    fn arrived(&mut self, from: &str, describe: impl FnOnce() -> String) {
        self.packets += 1;
        self.last_packet = Instant::now();
        self.silent_reported = false;
        if self.sender.as_deref() != Some(from) {
            if self.live {
                print!("\r{:width$}\r", "", width = STATUS_WIDTH);
            }
            println!("Receiving from {from} ({})", describe());
            self.sender = Some(from.to_string());
        }
    }

    /// The connection ended: the next sound announces its sender again.
    fn forget_sender(&mut self) {
        self.sender = None;
    }

    fn tick(&mut self, jitter: &Buffers, peak: &AtomicU32) {
        let (stats, buffered) = jitter.state();
        if self.live && self.last_status.elapsed() >= STATUS_EVERY {
            let seconds = self.last_status.elapsed().as_secs_f32();
            let level = f32::from_bits(peak.swap(0, Ordering::Relaxed));
            let waiting = self.last_packet.elapsed() > Duration::from_secs(1);
            let line = status_line(self.sender.as_deref(), waiting, self.packets as f32 / seconds, buffered * 1000 / self.rate as usize, &stats, &self.shown, level);
            print!("\r{line}");
            let _ = std::io::stdout().flush();
            (self.last_status, self.packets) = (Instant::now(), 0);
            if stats != self.shown && self.last_report.elapsed() >= REPORT_EVERY {
                (self.shown, self.last_report) = (stats, Instant::now());
            }
        } else if !self.live {
            if self.last_report.elapsed() >= REPORT_EVERY && stats != self.reported {
                report(&self.reported, &stats);
                self.reported = stats;
                self.last_report = Instant::now();
            }
            if !self.silent_reported && self.last_packet.elapsed() > Duration::from_secs(5) {
                println!("No sound arriving (sender stopped or paused)");
                self.silent_reported = true;
            }
        }
    }
}

const STATUS_EVERY: Duration = Duration::from_millis(250);
const STATUS_WIDTH: usize = 100;

/// The live status line: who, how much, how full the buffer is, problems, and a level meter.
fn status_line(sender: Option<&str>, waiting: bool, rate: f32, buffer_ms: usize, now: &Stats, since: &Stats, level: f32) -> String {
    let line = match sender {
        None => "Waiting for sound…".to_string(),
        Some(_) if waiting => format!("Waiting for sound… (problems so far: {} lost, {} dropouts)", now.lost, now.underruns),
        Some(from) => {
            let db = 20.0 * level.max(1e-6).log10();
            let bars = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * 20.0).round() as usize;
            let problems = (now.lost - since.lost) + (now.late - since.late) + (now.underruns - since.underruns) + (now.trimmed - since.trimmed);
            format!(
                "{from}  {rate:>3.0} pkt/s  buffer {buffer_ms:>3} ms  {}  [{}{}] {:>4}",
                if problems > 0 { format!("{problems} problem(s) just now") } else { "ok".to_string() },
                "█".repeat(bars),
                " ".repeat(20 - bars),
                if level > 1e-4 { format!("{db:.0}dB") } else { "--".to_string() }
            )
        }
    };
    format!("{line:width$}", width = STATUS_WIDTH)
}

const REPORT_EVERY: Duration = Duration::from_secs(5);

/// One line of what went wrong since the last report.
fn report(before: &Stats, now: &Stats) {
    let mut parts = Vec::new();
    for (count, what) in [
        (now.lost - before.lost, "packets lost on the network"),
        (now.late - before.late, "packets arrived too late"),
        (now.underruns - before.underruns, "dropouts (packets came too slowly: try a bigger --latency)"),
        (now.trimmed - before.trimmed, "skips (packets came in a burst)"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {what}"));
        }
    }
    println!("Last {} s: {}", REPORT_EVERY.as_secs(), parts.join(", "));
}

/// A UDP socket with a big receive buffer: Windows' default is small enough that a short
/// hiccup in this thread overflows it and loses packets.
fn bind(port: u16, group: Option<Ipv4Addr>) -> std::io::Result<UdpSocket> {
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

/// Join `group` on every IPv4 network this computer is on: joining with no interface uses only
/// the default route's, so on a computer with several networks (Wi-Fi and cable, a VPN, virtual
/// machines) the group's sound could arrive on another one and never be heard.
fn join_everywhere(socket: &Socket, group: Ipv4Addr) -> std::io::Result<()> {
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

/// What the sound card callback needs.
struct Output {
    jitter: Arc<Mutex<Jitter>>,
    peak: Arc<AtomicU32>,
    volume: f32,
    input_rate: u32,
}

fn play<T>(device: &cpal::Device, config: &StreamConfig, out: Output) -> Result<cpal::Stream, Box<dyn Error>>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut player = Player::new(out.input_rate, config.sample_rate);
    let Output { jitter, peak, volume, .. } = out;
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let mut jitter = jitter.lock().unwrap();
            player.update_speed(&jitter);
            let mut loudest = 0.0f32;
            for frame in data.chunks_mut(channels) {
                let [left, right] = player.next_frame(&mut jitter);
                // The meter shows what arrives, whatever the volume.
                loudest = loudest.max(left.abs()).max(right.abs());
                let (left, right) = ((left * volume).clamp(-1.0, 1.0), (right * volume).clamp(-1.0, 1.0));
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let value = match (channel, channels) {
                        (_, 1) => (left + right) * 0.5,
                        (0, _) => left,
                        (1, _) => right,
                        _ => 0.0,
                    };
                    *sample = T::from_sample(value);
                }
            }
            // Positive f32s order like their bits, so fetch_max keeps the loudest.
            peak.fetch_max(loudest.to_bits(), Ordering::Relaxed);
        },
        |err| eprintln!("sound card: {err}"),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::Unpacker;
    use crate::rtp;
    use crate::secure::ReplayWindow;

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
        let mut unpacker = Unpacker { key: None, replay: ReplayWindow::default(), decoder: None, channels: 2, last_opus: None, opus_frame: 960, warned: None };
        let frames = |u: &mut Unpacker, data: &[u8]| -> Vec<(u16, usize)> {
            u.unpack(data, &rtp::parse(data).unwrap()).into_iter().map(|(s, pcm)| (s, pcm.len())).collect()
        };
        assert_eq!(frames(&mut unpacker, &first), [(0, 3840)]);
        // Packet 1 never arrives: it's concealed (same length) right before packet 2.
        assert_eq!(frames(&mut unpacker, &third), [(1, 3840), (2, 3840)]);
    }

    #[test]
    fn websocket_urls() {
        let t = super::WsTarget::parse("ws://localhost:46080").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.request.as_str()), ("localhost", 46080, "ws://localhost:46080/ws"));
        let t = super::WsTarget::parse("ws://10.0.0.2/audio/").unwrap();
        assert_eq!((t.port, t.request.as_str()), (80, "ws://10.0.0.2/audio/ws"));
        assert_eq!(super::WsTarget::parse("ws://h:1/custom").unwrap().request, "ws://h:1/custom");
        assert!(super::WsTarget::parse("wss://h/").is_err());
        assert!(super::WsTarget::parse("ws://:5").is_err());
        assert!(super::WsTarget::parse("http://h").is_err());
    }
}
