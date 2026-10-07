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

/// Turns arriving packets (L16, Opus, or encrypted) into big-endian PCM for the jitter buffer,
/// in two steps: `open` checks a packet is for us (decrypting it with a key), `frames` decodes it.
/// In between, the caller decides whether its sender is the one being played.
struct Unpacker {
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
const MAX_CONCEAL: u16 = 5;

/// Payload types played as L16 (16-bit big-endian PCM): rtp-audio's own, the static L16 types
/// (10 stereo, 11 mono), and the dynamic range other tools use for it (PulseAudio, PipeWire,
/// ffmpeg). Other static types (PCMU, PCMA, …) are other codecs, not sound we can play as is.
fn is_l16(payload_type: u8) -> bool {
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

/// The outputs, each real one once. ALSA lists every card port under several names (hw:,
/// plughw:, hdmi:, dmix:, sysdefault:, …): a card's hw: ports are kept, one per port even when
/// two share a name (two HDMI ports to the same model of monitor), and its other aliases are left
/// out. Outputs that aren't a card (PipeWire, default) and those of other systems are kept, each
/// ID once.
fn outputs() -> Result<Vec<Outlet>, Box<dyn Error>> {
    let all: Vec<Outlet> = cpal::default_host()
        .output_devices()?
        .map(|device| Outlet { name: device_name(&device), id: device_id(&device), device })
        .collect();
    let hw_cards: std::collections::HashSet<String> =
        all.iter().filter_map(|o| alsa_port(&o.id)).filter(|port| port.plugin == "hw").map(|port| port.card).collect();
    let mut seen = std::collections::HashSet::new();
    Ok(all
        .into_iter()
        // ALSA plugins that only convert, mix channels or lead to other systems: not outputs.
        .filter(|o| !ALSA_HELPERS.iter().any(|helper| o.id == format!("alsa:{helper}")))
        .filter(|o| match alsa_port(&o.id) {
            // A card's port once: hw:CARD=NVidia,DEV=3 and hw:CARD=1,DEV=3 are the same one.
            Some(port) => {
                (port.plugin == "hw" || !hw_cards.contains(&port.card)) && seen.insert(format!("{}:{}:{}", port.plugin, port.card, port.dev))
            }
            // Without an ID there's no telling two apart: keep them all.
            None => o.id.is_empty() || seen.insert(o.id.clone()),
        })
        .collect())
}

/// ALSA plugins listed as outputs that aren't places to play: rate converters, channel
/// up/down-mixers, effects, and bridges to JACK and OSS.
const ALSA_HELPERS: &[&str] = &["lavrate", "samplerate", "speexrate", "speex", "upmix", "vdownmix", "jack", "oss"];

/// An ALSA card port, from an ID like alsa:hw:CARD=NVidia,DEV=3.
struct AlsaPort {
    plugin: String,
    /// The card's name; a card given by number (CARD=1) is looked up, so both spellings match.
    card: String,
    dev: String,
}

fn alsa_port(id: &str) -> Option<AlsaPort> {
    let rest = id.strip_prefix("alsa:")?;
    let (plugin, args) = rest.split_once(':')?;
    let field = |name: &str| args.split(',').find_map(|arg| arg.strip_prefix(name)).map(str::to_string);
    let mut card = field("CARD=")?;
    if card.chars().all(|c| c.is_ascii_digit())
        && let Ok(name) = std::fs::read_to_string(format!("/proc/asound/card{card}/id"))
    {
        card = name.trim().to_string();
    }
    Some(AlsaPort { plugin: plugin.to_string(), card, dev: field("DEV=").unwrap_or_default() })
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
    if let Some(i) = outputs.iter().position(|o| o.id == wanted) {
        return Ok(outputs.swap_remove(i).device);
    }
    let named: Vec<usize> = (0..outputs.len()).filter(|&i| outputs[i].name.to_lowercase() == lower).collect();
    match named.as_slice() {
        [i] => return Ok(outputs.swap_remove(*i).device),
        [] => {}
        _ => {
            return Err(format!(
                "several outputs are called '{wanted}': choose one by its ID ({})",
                named.iter().map(|&i| outputs[i].id.as_str()).collect::<Vec<_>>().join(", ")
            )
            .into());
        }
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
    // An exact ID, or a name only one output has (a shared name is left to pick_device, which
    // asks for the ID).
    let exact = |text: &str| {
        outputs.iter().position(|o| o.id == text).or_else(|| {
            let mut named = outputs.iter().enumerate().filter(|(_, o)| o.name.eq_ignore_ascii_case(text));
            match (named.next(), named.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        })
    };
    let mut devices = Vec::new();
    for value in wanted {
        // A name several outputs share, given whole: ask which one (by ID) rather than read it as
        // a list.
        if outputs.iter().filter(|o| o.name.eq_ignore_ascii_case(value.trim())).count() > 1 {
            pick_device(Some(value.trim()))?;
        }
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

/// How much a card opened directly (ALSA hw:) is asked for at a time: 20 ms. Left to itself,
/// ALSA can pick seconds, which the jitter buffer can't keep level with; 10 ms was too tight for a
/// card playing next to other outputs. Outputs that go through a sound server (PipeWire,
/// PulseAudio, the default) keep their own sizes: a short period runs them dry.
const CARD_PERIOD_MS: u32 = 20;

/// Start playing on `device`; returns the stream and a description of it.
fn open_output(device: &cpal::Device, out: Output) -> Result<(cpal::Stream, String), Box<dyn Error>> {
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let id = device_id(device);
    // A card opened directly, or an output of the sound server through its own protocol (which
    // then keeps that latency, asking for sound steadily rather than in big bursts).
    let direct = alsa_port(&id).is_some_and(|port| port.plugin == "hw") || id.starts_with("pulseaudio:");
    let buffer_size = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } if direct => {
            cpal::BufferSize::Fixed((supported.sample_rate() * CARD_PERIOD_MS / 1000).clamp(*min, *max))
        }
        _ => cpal::BufferSize::Default,
    };
    let mut config: StreamConfig = supported.into();
    config.buffer_size = buffer_size;
    let stream = match format {
        SampleFormat::F32 => play::<f32>(device, &config, out),
        SampleFormat::I16 => play::<i16>(device, &config, out),
        SampleFormat::U16 => play::<u16>(device, &config, out),
        SampleFormat::I32 => play::<i32>(device, &config, out),
        SampleFormat::F64 => play::<f64>(device, &config, out),
        other => return Err(format!("sample format {other} is not supported").into()),
    }?;
    stream.play()?;
    Ok((stream, format!("{} ({} Hz, {} ch, {format})", device_name(device), config.sample_rate, config.channels)))
}

/// The jitter buffers of all the outputs: every frame goes into each.
struct Buffers(Vec<(String, Arc<Mutex<Jitter>>)>);

impl Buffers {
    fn push(&self, sequence: u16, pcm: &[u8], channels: usize) {
        for (_, jitter) in &self.0 {
            jitter.lock().unwrap().push(sequence, pcm, channels);
        }
    }

    /// Packets lost on the way that the jitter buffers saw no gap for (concealed by Opus).
    fn count_lost(&self, packets: u16) {
        for (_, jitter) in &self.0 {
            jitter.lock().unwrap().stats.lost += u64::from(packets);
        }
    }

    /// A new sender: drop what's buffered from the last one.
    fn restart(&self) {
        for (_, jitter) in &self.0 {
            jitter.lock().unwrap().restart();
        }
    }

    /// For the status line and reports: each output's totals, and the emptiest buffer.
    fn state(&self) -> (Vec<Stats>, usize) {
        let mut buffered = usize::MAX;
        let stats = self
            .0
            .iter()
            .map(|(_, jitter)| {
                let jitter = jitter.lock().unwrap();
                buffered = buffered.min(jitter.buffered());
                jitter.stats
            })
            .collect();
        (stats, if buffered == usize::MAX { 0 } else { buffered })
    }

    /// Short names of the outputs, to say which one has problems.
    fn labels(&self) -> Vec<&str> {
        self.0.iter().map(|(label, _)| label.as_str()).collect()
    }
}

/// Receive RTP audio on a UDP port, or the sender's WebSocket stream, and play it on a sound
/// card until killed.
pub fn run(options: Options) -> Result<(), Box<dyn Error>> {
    // In 64 bits: the product of two accepted values can exceed 32.
    let target = (u64::from(options.rate) * u64::from(options.latency_ms) / 1000).max(1) as usize;
    // Loudest sample played since the status line last looked, as f32 bits.
    let peak = Arc::new(AtomicU32::new(0));
    // One buffer per output: each sound card runs on its own clock, so each keeps its own level.
    let (mut buffers, mut streams, mut cards) = (Vec::new(), Vec::new(), Vec::new());
    let devices = pick_devices(&options.devices)?;
    let several = devices.len() > 1;
    let mut first_error = None;
    for device in devices {
        let jitter = Arc::new(Mutex::new(Jitter::new(target)));
        let out = Output { jitter: Arc::clone(&jitter), peak: Arc::clone(&peak), volume: options.volume, input_rate: options.rate };
        match open_output(&device, out) {
            Ok((stream, card)) => {
                cards.push((device_name(&device), card));
                buffers.push((short_name(&device_name(&device)), jitter));
                streams.push(stream);
            }
            // With several outputs, one that won't open is skipped: the others still play.
            Err(err) if several => {
                eprintln!("Skipping {}: {err}", device_name(&device));
                first_error.get_or_insert(err);
            }
            Err(err) => return Err(err),
        }
    }
    if streams.is_empty() {
        return Err(first_error.unwrap_or_else(|| "no output could be opened".into()));
    }
    // One output: its details. Several: just their names, on one short line.
    let card = match cards.as_slice() {
        [(_, details)] => format!("sound card: {details}"),
        _ => format!("playing on {} outputs: {}", cards.len(), cards.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ")),
    };
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
const SENDER_QUIET: Duration = Duration::from_secs(1);

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
        // An IPv6 address comes in brackets ([::1]:46080), since it has colons of its own.
        let (host, port) = if let Some(inside) = authority.strip_prefix('[') {
            let (host, after) = inside.split_once(']').ok_or_else(|| format!("unclosed [ in '{url}'"))?;
            let port = match after.strip_prefix(':') {
                Some(port) => port.parse::<u16>().map_err(|_| format!("bad port in '{url}'"))?,
                None if after.is_empty() => 80,
                None => return Err(format!("bad address in '{url}'")),
            };
            (host, port)
        } else {
            match authority.split_once(':') {
                Some((_, port)) if port.contains(':') => {
                    return Err(format!("put an IPv6 address in brackets: ws://[{authority}]/ or ws://[ADDRESS]:PORT"));
                }
                Some((host, port)) => (host, port.parse::<u16>().map_err(|_| format!("bad port in '{url}'"))?),
                None => (authority, 80),
            }
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
    /// Per output, the totals the status line and the reports last counted from.
    shown: Vec<Stats>,
    last_report: Instant,
    reported: Vec<Stats>,
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
            shown: Vec::new(),
            last_report: now,
            reported: Vec::new(),
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
        self.shown.resize(stats.len(), Stats::default());
        self.reported.resize(stats.len(), Stats::default());
        let labels = jitter.labels();
        if self.live && self.last_status.elapsed() >= STATUS_EVERY {
            let seconds = self.last_status.elapsed().as_secs_f32();
            let level = f32::from_bits(peak.swap(0, Ordering::Relaxed));
            let waiting = self.last_packet.elapsed() > Duration::from_secs(1);
            let line = status_line(
                self.sender.as_deref(),
                waiting,
                self.packets as f32 / seconds,
                buffered * 1000 / self.rate as usize,
                Problems { labels: &labels, now: &stats, since: &self.shown },
                level,
            );
            print!("\r{line}");
            let _ = std::io::stdout().flush();
            (self.last_status, self.packets) = (Instant::now(), 0);
            if stats != self.shown && self.last_report.elapsed() >= REPORT_EVERY {
                (self.shown, self.last_report) = (stats, Instant::now());
            }
        } else if !self.live {
            if self.last_report.elapsed() >= REPORT_EVERY && stats != self.reported {
                report(Problems { labels: &labels, now: &stats, since: &self.reported });
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

/// What went wrong on each output since some earlier totals. Network problems (lost, late) are
/// the same for every output, so they're counted once; playing problems belong to an output.
struct Problems<'a> {
    labels: &'a [&'a str],
    now: &'a [Stats],
    since: &'a [Stats],
}

impl Problems<'_> {
    /// Network problems: lost and late packets.
    fn network(&self) -> (u64, u64) {
        match (self.now.first(), self.since.first()) {
            (Some(now), Some(since)) => (now.lost - since.lost, now.late - since.late),
            _ => (0, 0),
        }
    }

    /// Playing problems of each output: dropouts, skips, sound card hiccups.
    fn playing(&self) -> impl Iterator<Item = (&str, u64, u64, u64)> + '_ {
        self.labels.iter().zip(self.now.iter().zip(self.since)).map(|(label, (now, since))| {
            (*label, now.underruns - since.underruns, now.trimmed - since.trimmed, now.card - since.card)
        })
    }

    fn total(&self) -> u64 {
        let (lost, late) = self.network();
        lost + late + self.playing().map(|(_, a, b, c)| a + b + c).sum::<u64>()
    }

    /// The outputs with playing problems, when there are several outputs.
    fn troubled(&self) -> Vec<&str> {
        if self.labels.len() < 2 {
            return Vec::new();
        }
        self.playing().filter(|(_, a, b, c)| a + b + c > 0).map(|(label, ..)| label).collect()
    }
}

/// The live status line: who, how much, how full the buffer is, problems (and on which output,
/// with several), and a level meter.
fn status_line(sender: Option<&str>, waiting: bool, rate: f32, buffer_ms: usize, problems: Problems, level: f32) -> String {
    let line = match sender {
        None => "Waiting for sound…".to_string(),
        Some(_) if waiting => {
            let lost = problems.now.iter().map(|s| s.lost).max().unwrap_or(0);
            let dropouts = problems.now.iter().map(|s| s.underruns).max().unwrap_or(0);
            format!("Waiting for sound… (problems so far: {lost} lost, {dropouts} dropouts)")
        }
        Some(from) => {
            let db = 20.0 * level.max(1e-6).log10();
            let bars = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * 20.0).round() as usize;
            let total = problems.total();
            let troubled = problems.troubled();
            let state = match (total, troubled.as_slice()) {
                (0, _) => "ok".to_string(),
                (n, []) => format!("{n} problem(s) just now"),
                (n, outputs) => format!("{n} problem(s) just now ({})", outputs.join(", ")),
            };
            format!(
                "{from}  {rate:>3.0} pkt/s  buffer {buffer_ms:>3} ms  {state}  [{}{}] {:>4}",
                "█".repeat(bars),
                " ".repeat(20 - bars),
                if level > 1e-4 { format!("{db:.0}dB") } else { "--".to_string() }
            )
        }
    };
    format!("{line:width$}", width = STATUS_WIDTH)
}

const REPORT_EVERY: Duration = Duration::from_secs(5);

/// One line of what went wrong since the last report; with several outputs, each output's
/// playing problems under its name.
fn report(problems: Problems) {
    let mut parts = Vec::new();
    let (lost, late) = problems.network();
    for (count, what) in [(lost, "packets lost on the network"), (late, "packets arrived too late")] {
        if count > 0 {
            parts.push(format!("{count} {what}"));
        }
    }
    let several = problems.labels.len() > 1;
    for (label, dropouts, skips, card) in problems.playing() {
        let mut own = Vec::new();
        for (count, what) in [
            (dropouts, "dropouts (packets came too slowly: try a bigger --latency)"),
            (skips, "skips (packets came in a burst)"),
            (card, "sound card underruns or overruns"),
        ] {
            if count > 0 {
                own.push(format!("{count} {what}"));
            }
        }
        if !own.is_empty() {
            parts.push(if several { format!("{label}: {}", own.join(", ")) } else { own.join(", ") });
        }
    }
    println!("Last {} s: {}", REPORT_EVERY.as_secs(), parts.join("; "));
}

/// A short name for an output on the status line: the part in brackets at the end ("… Digital
/// Stereo (HDMI 2)" → "HDMI 2"), else the port after the card ("HDA NVidia, HDMI 3" → "HDMI 3"),
/// else the name; at most 24 characters.
fn short_name(name: &str) -> String {
    let short = name
        .strip_suffix(')')
        .and_then(|rest| rest.rfind('(').map(|i| &rest[i + 1..]))
        .filter(|inside| !inside.is_empty())
        .or_else(|| name.rsplit_once(", ").map(|(_, port)| port))
        .unwrap_or(name);
    short.chars().take(24).collect()
}

/// A UDP socket with a big receive buffer: Windows' default is small enough that a short
/// hiccup in this thread overflows it and loses packets.
fn bind(port: u16, group: Option<Ipv4Addr>) -> std::io::Result<UdpSocket> {
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
fn bind_dual_stack(port: u16) -> std::io::Result<UdpSocket> {
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
    let errors = Arc::clone(&jitter);
    let name = device_name(device);
    let mut shown = false;
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
        // Counted with the other problems (status line, reports); only the first is spelled out,
        // on a line of its own, so a card that keeps hiccuping doesn't flood the terminal.
        move |err| {
            errors.lock().unwrap().stats.card += 1;
            if !shown {
                shown = true;
                eprintln!("\r{:width$}\r{name}: {err} (further ones are counted as problems)", "", width = STATUS_WIDTH);
            }
        },
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::Unpacker;
    use crate::rtp;

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

    #[test]
    fn short_output_names() {
        assert_eq!(super::short_name("GA106 High Definition Audio Controller Digital Stereo (HDMI 2)"), "HDMI 2");
        assert_eq!(super::short_name("Bose Flex SoundLink"), "Bose Flex SoundLink");
        assert_eq!(super::short_name("HD-Audio Generic, ALC897 Analog"), "ALC897 Analog");
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
        let t = super::WsTarget::parse("ws://[::1]:46080").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.request.as_str()), ("::1", 46080, "ws://[::1]:46080/ws"));
        assert_eq!(super::WsTarget::parse("ws://[fe80::1]/audio/").unwrap().port, 80);
        assert!(super::WsTarget::parse("ws://::1:46080").is_err()); // needs brackets
        assert!(super::WsTarget::parse("ws://[::1").is_err());
    }
}

