//! The receiver: plays what a sender sends, over UDP or over TCP (ws://), on one or several outputs.

mod jitter;
mod outputs;
mod spectrum;
mod status;
mod udp;
mod websocket;

pub use outputs::list_devices;

use std::error::Error;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use jitter::{Feed, Stats};
use outputs::{Output, device_name, open_output, pick_devices, short_name};
use status::Monitor;
use udp::receive_udp;
use websocket::receive_websocket;

use crate::net::secure::Key;

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
    /// The status as JSON lines, for a program running the receiver.
    pub json: bool,
    /// Line the outputs up by the latency each reports.
    pub sync: bool,
}

/// The jitter buffers of all the outputs: every frame goes into each (and into the spectrum, with
/// --json).
struct Buffers(Vec<(String, Feed)>, Option<spectrum::Tap>);

impl Buffers {
    fn push(&mut self, sequence: u16, pcm: &[u8], channels: usize) {
        // Feed every output first; visualization is optional and never backpressures audio.
        for (_, jitter) in &mut self.0 {
            jitter.push(sequence, pcm, channels);
        }
        if let Some(spectrum) = &self.1 {
            spectrum.feed(pcm, channels);
        }
    }

    /// Packets lost on the way that the jitter buffers saw no gap for (concealed by Opus).
    fn count_lost(&self, packets: u16) {
        for (_, jitter) in &self.0 {
            jitter.count_lost(packets);
        }
    }

    /// A new sender: drop what's buffered from the last one.
    fn restart(&mut self) {
        for (_, jitter) in &mut self.0 {
            jitter.restart();
        }
    }

    /// For the status line and reports: each output's totals, and the emptiest buffer.
    fn state(&self) -> (Vec<Stats>, usize) {
        let mut buffered = usize::MAX;
        let stats = self
            .0
            .iter()
            .map(|(_, jitter)| {
                let (stats, waiting) = jitter.metrics.state();
                buffered = buffered.min(waiting);
                stats
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
    let (mut buffers, mut streams, mut cards, mut ids) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let devices = pick_devices(&options.devices)?;
    let several = devices.len() > 1;
    let mut first_error = None;
    for (device, tuning) in devices {
        // A delayed output keeps that much more sound buffered, so it plays that much later.
        let delayed = target + (u64::from(options.rate) * u64::from(tuning.delay_ms) / 1000) as usize;
        let (feed, jitter, errors) = jitter::channel(delayed);
        let volume = options.volume * tuning.volume;
        let out = Output { jitter, errors, peak: Arc::clone(&peak), volume, input_rate: options.rate };
        match open_output(&device, out) {
            Ok((stream, card)) => {
                let mut delay = if tuning.delay_ms > 0 { format!(" +{} ms", tuning.delay_ms) } else { String::new() };
                if tuning.volume != 1.0 {
                    delay += &format!(" {:.0}%", tuning.volume * 100.0);
                }
                cards.push((format!("{}{delay}", device_name(&device)), format!("{card}{delay}")));
                buffers.push((short_name(&device_name(&device)), feed));
                ids.push(outputs::device_id(&device));
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
    // Two outputs with the same name (two monitors of one model) are told apart by their IDs.
    for i in 0..buffers.len() {
        if buffers.iter().filter(|(label, _)| *label == buffers[i].0).count() > 1 {
            buffers[i].0 = format!("{} [{}]", buffers[i].0, ids[i]);
        }
    }
    let mut jitter = Buffers(buffers, options.json.then(|| spectrum::Tap::new(options.rate)));
    if options.sync && cards.len() > 1 {
        println!("Lining the outputs up by the latency each reports (--sync); a Bluetooth speaker's own delay isn't reported: add it with +NNms");
    }
    let mut monitor = Monitor::new(options.rate, options.json.then_some(ids), options.sync);
    match &options.url {
        Some(url) => receive_websocket(url, &options, &card, &mut jitter, &peak, &mut monitor),
        None => receive_udp(&options, &card, &mut jitter, &peak, &mut monitor),
    }
}

fn print_volume_and_quit(options: &Options) {
    if options.volume != 1.0 {
        println!("Volume: {:.0}%", options.volume * 100.0);
    }
    println!("Ctrl+C to quit.");
}
