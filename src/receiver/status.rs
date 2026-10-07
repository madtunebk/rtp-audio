//! What the receiver tells: the live status line at a terminal, or reports in a log.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::Buffers;
use super::jitter::Stats;

struct Snapshot {
    stats: Vec<Stats>,
    buffered: usize,
    labels: Vec<String>,
    /// Per output: sound buffered here, the device's own latency, and what --sync adds, in ms.
    timing: Vec<(f32, f32, f32)>,
    spectrum: Option<[f32; super::spectrum::BANDS]>,
    level: f32,
    packets: u32,
    /// Sound received so far (codec payload, no headers), in bytes.
    bytes: u64,
    sender: Option<String>,
    description: String,
    heard: Instant,
    errors: Vec<(String, String)>,
}
/// Reception only publishes bounded snapshots. A blocked stdout pipe cannot stall audio.
pub(super) struct Monitor {
    pub(super) live: bool,
    publish: std::sync::mpsc::SyncSender<Snapshot>,
    sender: Option<String>,
    description: String,
    packets: u32,
    bytes: u64,
    heard: Instant,
    last: Instant,
    rate: u32,
    /// With --sync: each output's device latency, smoothed (ms).
    sync: Option<Vec<f32>>,
}
impl Monitor {
    pub(super) fn new(rate: u32, json: Option<Vec<String>>, sync: bool) -> Self {
        let mut reporter = Reporter::new(rate, json);
        let live = reporter.live;
        let (publish, receiver) = std::sync::mpsc::sync_channel(2);
        std::thread::spawn(move || {
            while let Ok(snapshot) = receiver.recv() {
                reporter.tick(snapshot);
            }
        });
        let now = Instant::now();
        Self { live, publish, sender: None, description: String::new(), packets: 0, bytes: 0, heard: now, last: now, rate, sync: sync.then(Vec::new) }
    }
    /// A packet of `bytes` of sound (its codec payload) arrived from `from`.
    pub(super) fn arrived(&mut self, from: &str, bytes: usize, describe: impl FnOnce() -> String) {
        self.packets = self.packets.saturating_add(1);
        self.bytes += bytes as u64;
        self.heard = Instant::now();
        if self.sender.as_deref() != Some(from) {
            self.sender = Some(from.to_owned());
            self.description = describe();
        }
    }
    pub(super) fn forget_sender(&mut self) {
        self.sender = None;
    }
    pub(super) fn tick(&mut self, jitter: &mut Buffers, peak: &AtomicU32) {
        if self.last.elapsed() < SPECTRUM_EVERY {
            return;
        }
        self.last = Instant::now();
        if let Some(smoothed) = &mut self.sync {
            align(smoothed, jitter, self.rate);
        }
        let (stats, buffered) = jitter.state();
        let labels = jitter.labels().iter().map(|label| (*label).to_owned()).collect();
        let mut errors = Vec::new();
        for (name, feed) in &mut jitter.0 {
            // Bounded diagnostic ring; formatting is outside the callback.
            for _ in 0..16 {
                match feed.errors.pop() {
                    Ok(error) => errors.push((name.clone(), error.to_string())),
                    Err(_) => break,
                }
            }
        }
        let snapshot = Snapshot {
            stats,
            buffered,
            labels,
            timing: jitter
                .0
                .iter()
                .map(|(_, feed)| {
                    let ms = |frames: usize| frames as f32 * 1000.0 / self.rate as f32;
                    let sync = feed.metrics.sync_frames.load(Ordering::Relaxed);
                    (ms(feed.metrics.state().1), feed.metrics.device_latency_ms(), ms(sync))
                })
                .collect(),
            spectrum: jitter.1.as_ref().and_then(|tap| tap.latest()),
            level: f32::from_bits(peak.swap(0, Ordering::Relaxed)),
            packets: self.packets,
            bytes: self.bytes,
            sender: self.sender.clone(),
            description: self.description.clone(),
            heard: self.heard,
            errors,
        };
        let _ = self.publish.try_send(snapshot);
    }
}

/// The live status line at a terminal, or occasional reports in a log, for either source.
struct Reporter {
    pub(super) live: bool,
    /// A JSON status line every JSON_EVERY instead of the status line or the reports, naming the
    /// outputs by these IDs.
    json: Option<Vec<String>>,
    rate: u32,
    sender: Option<String>,
    packets: u32,
    total_packets: u32,
    /// Sound bytes since the status was last shown, and in all.
    bytes: u64,
    total_bytes: u64,
    level_peak: f32,
    announced_errors: std::collections::HashSet<String>,
    last_status: Instant,
    /// Per output, the totals the status line and the reports last counted from.
    shown: Vec<Stats>,
    last_spectrum: Instant,
    last_report: Instant,
    reported: Vec<Stats>,
    last_packet: Instant,
    silent_reported: bool,
}

impl Reporter {
    pub(super) fn new(rate: u32, json: Option<Vec<String>>) -> Self {
        let now = Instant::now();
        Reporter {
            // At a terminal, one status line updates in place; in a log, only events and problems.
            live: json.is_none() && std::io::stdout().is_terminal(),
            json,
            rate,
            sender: None,
            packets: 0,
            total_packets: 0,
            bytes: 0,
            total_bytes: 0,
            level_peak: 0.,
            announced_errors: std::collections::HashSet::new(),
            last_status: now,
            shown: Vec::new(),
            last_spectrum: now,
            last_report: now,
            reported: Vec::new(),
            last_packet: now,
            silent_reported: true,
        }
    }

    fn tick(&mut self, snapshot: Snapshot) {
        let Snapshot { stats, buffered, labels, timing, spectrum, level, packets, bytes, sender, description, heard, errors } = snapshot;
        self.packets += packets.saturating_sub(self.total_packets);
        self.total_packets = packets;
        self.bytes += bytes.saturating_sub(self.total_bytes);
        self.total_bytes = bytes;
        if heard != self.last_packet {
            self.silent_reported = false;
        }
        self.last_packet = heard;
        if sender != self.sender {
            if let Some(from) = &sender {
                if self.live {
                    print!("\r{:width$}\r", "", width = STATUS_WIDTH);
                }
                println!("Receiving from {from} ({description})");
            }
            self.sender = sender;
        }
        for (device, error) in errors {
            if self.announced_errors.insert(format!("{device}:{error}")) {
                eprintln!("{device}: {error} (further ones are counted as problems)");
            }
        }
        self.level_peak = self.level_peak.max(level);
        self.shown.resize(stats.len(), Stats::default());
        self.reported.resize(stats.len(), Stats::default());
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        if let Some(ids) = &self.json {
            // While sound arrives, its spectrum 20 times a second: {"spectrum":[0.12,…]}.
            if let Some(bands) = spectrum
                && self.last_spectrum.elapsed() >= SPECTRUM_EVERY
                && self.last_packet.elapsed() < SPECTRUM_EVERY
            {
                println!("{{\"spectrum\":[{}]}}", bands.iter().map(|b| format!("{b:.2}")).collect::<Vec<_>>().join(","));
                self.last_spectrum = Instant::now();
            }
            if self.last_status.elapsed() >= JSON_EVERY {
                let seconds = self.last_status.elapsed().as_secs_f32();
                let level = std::mem::replace(&mut self.level_peak, 0.);
                let waiting = self.last_packet.elapsed() > Duration::from_secs(1);
                let rate = self.packets as f32 / seconds;
                let kbps = self.bytes as f32 * 8.0 / 1000.0 / seconds;
                println!("{}", json_status(self.sender.as_deref(), waiting, rate, kbps, buffered * 1000 / self.rate as usize, &labels, ids, &stats, &timing, level));
                (self.last_status, self.packets, self.bytes) = (Instant::now(), 0, 0);
            }
        } else if self.live && self.last_status.elapsed() >= STATUS_EVERY {
            let seconds = self.last_status.elapsed().as_secs_f32();
            let level = std::mem::replace(&mut self.level_peak, 0.);
            let waiting = self.last_packet.elapsed() > Duration::from_secs(1);
            let line = status_line(
                self.sender.as_deref(),
                waiting,
                self.packets as f32 / seconds,
                self.bytes as f32 * 8.0 / 1000.0 / seconds,
                buffered * 1000 / self.rate as usize,
                Problems { labels: &labels, now: &stats, since: &self.shown },
                level,
            );
            print!("\r{line}");
            let _ = std::io::stdout().flush();
            (self.last_status, self.packets, self.bytes) = (Instant::now(), 0, 0);
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

pub(super) const STATUS_EVERY: Duration = Duration::from_millis(250);

/// How often --json prints the status, and the spectrum.
const JSON_EVERY: Duration = Duration::from_millis(500);
const SPECTRUM_EVERY: Duration = Duration::from_millis(50);

/// The status for a program: who sends, packets per second, the sound's bit rate (kbit/s, codec
/// payload), the emptiest buffer, the level (dB,
/// null when silent), the network problems and each output's playing problems (by its ID from
/// `devices --json`), as totals since the start, on one line:
/// {"status":{"sender":"192.168.1.5:40000","waiting":false,"packets":50,"kbps":128,"buffer_ms":60,"level_db":-12.3,
///  "lost":0,"late":0,"outputs":[{"name":"HDMI 2","id":"alsa:hw:…","dropouts":0,"skips":0,"card":0}]}}
#[allow(clippy::too_many_arguments)]
fn json_status(
    sender: Option<&str>,
    waiting: bool,
    rate: f32,
    kbps: f32,
    buffer_ms: usize,
    labels: &[&str],
    ids: &[String],
    stats: &[Stats],
    timing: &[(f32, f32, f32)],
    level: f32,
) -> String {
    use crate::json::string;
    let sender = sender.map_or("null".to_string(), string);
    let level = if level > 1e-4 { format!("{:.1}", 20.0 * level.log10()) } else { "null".to_string() };
    let (lost, late) = stats.first().map_or((0, 0), |s| (s.lost, s.late));
    let outputs: Vec<String> = labels
        .iter()
        .zip(ids)
        .zip(stats)
        .zip(timing)
        .map(|(((name, id), s), (buffer_ms, device_ms, sync_ms))| {
            format!(
                r#"{{"name":{},"id":{},"buffer_ms":{buffer_ms:.1},"device_ms":{device_ms:.1},"sync_ms":{sync_ms:.1},"dropouts":{},"skips":{},"card":{}}}"#,
                string(name),
                string(id),
                s.underruns,
                s.trimmed,
                s.card
            )
        })
        .collect();
    format!(
        r#"{{"status":{{"sender":{sender},"waiting":{waiting},"packets":{rate:.0},"kbps":{kbps:.0},"buffer_ms":{buffer_ms},"level_db":{level},"lost":{lost},"late":{late},"outputs":[{}]}}}}"#,
        outputs.join(",")
    )
}

pub(super) const STATUS_WIDTH: usize = 100;

/// --sync: makes every output wait as long as the slowest one takes to play, by the device latency
/// each reports (the time from its callback to the sound leaving it). That covers cards, HDMI and
/// USB; a Bluetooth speaker's own delay may not be (Windows doesn't), and stays for +NNms.
fn align(smoothed: &mut Vec<f32>, jitter: &Buffers, rate: u32) {
    let latencies: Vec<f32> = jitter.0.iter().map(|(_, feed)| feed.metrics.device_latency_ms()).collect();
    smoothed.resize(latencies.len(), 0.0);
    for (smooth, &latency) in smoothed.iter_mut().zip(&latencies) {
        // An output that hasn't told its latency yet (not playing, or a sound server still
        // measuring) keeps what it had. The reports jump by a period from one callback to the
        // next: follow them slowly.
        if latency > 0.0 {
            *smooth = if *smooth == 0.0 { latency } else { *smooth * 0.95 + latency * 0.05 };
        }
    }
    let slowest = smoothed.iter().copied().fold(0.0, f32::max);
    for ((_, feed), &smooth) in jitter.0.iter().zip(smoothed.iter()) {
        if smooth == 0.0 {
            continue;
        }
        let frames = ((slowest - smooth) * rate as f32 / 1000.0).round() as usize;
        let current = feed.metrics.sync_frames.load(Ordering::Relaxed);
        // Only a change of more than 2 ms moves it: each move is a little speed-up or slow-down.
        if frames.abs_diff(current) > rate as usize / 500 {
            feed.metrics.sync_frames.store(frames, Ordering::Relaxed);
        }
    }
}

/// What went wrong on each output since some earlier totals. Network problems (lost, late) are
/// the same for every output, so they're counted once; playing problems belong to an output.
pub(super) struct Problems<'a> {
    labels: &'a [&'a str],
    now: &'a [Stats],
    since: &'a [Stats],
}

impl Problems<'_> {
    /// Network problems: lost and late packets.
    pub(super) fn network(&self) -> (u64, u64) {
        match (self.now.first(), self.since.first()) {
            (Some(now), Some(since)) => (now.lost - since.lost, now.late - since.late),
            _ => (0, 0),
        }
    }

    /// Playing problems of each output: dropouts, skips, sound card hiccups.
    pub(super) fn playing(&self) -> impl Iterator<Item = (&str, u64, u64, u64)> + '_ {
        self.labels
            .iter()
            .zip(self.now.iter().zip(self.since))
            .map(|(label, (now, since))| (*label, now.underruns - since.underruns, now.trimmed - since.trimmed, now.card - since.card))
    }

    pub(super) fn total(&self) -> u64 {
        let (lost, late) = self.network();
        lost + late + self.playing().map(|(_, a, b, c)| a + b + c).sum::<u64>()
    }

    /// The outputs with playing problems, when there are several outputs.
    pub(super) fn troubled(&self) -> Vec<&str> {
        if self.labels.len() < 2 {
            return Vec::new();
        }
        self.playing().filter(|(_, a, b, c)| a + b + c > 0).map(|(label, ..)| label).collect()
    }
}

/// The live status line: who, how much (packets, and the sound's bit rate), how full the buffer is, problems (and on which output,
/// with several), and a level meter.
pub(super) fn status_line(sender: Option<&str>, waiting: bool, rate: f32, kbps: f32, buffer_ms: usize, problems: Problems, level: f32) -> String {
    let line = match sender {
        None => "Waiting for sound...".to_string(),
        Some(_) if waiting => {
            let lost = problems.now.iter().map(|s| s.lost).max().unwrap_or(0);
            let dropouts = problems.now.iter().map(|s| s.underruns).max().unwrap_or(0);
            format!("Waiting for sound... (problems so far: {lost} lost, {dropouts} dropouts)")
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
                "{from}  {rate:>3.0} pkt/s  {kbps:>4.0} kbit/s  buffer {buffer_ms:>3} ms  {state}  [{}{}] {:>4}",
                "█".repeat(bars),
                " ".repeat(20 - bars),
                if level > 1e-4 { format!("{db:.0}dB") } else { "--".to_string() }
            )
        }
    };
    format!("{line:width$}", width = STATUS_WIDTH)
}

pub(super) const REPORT_EVERY: Duration = Duration::from_secs(5);

/// One line of what went wrong since the last report; with several outputs, each output's
/// playing problems under its name.
pub(super) fn report(problems: Problems) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sync_makes_the_quicker_outputs_wait_for_the_slowest() {
        let outputs: Vec<_> = [40_000u64, 80_000, 45_000, 0]
            .iter()
            .map(|&latency_us| {
                let (feed, _, _) = crate::receiver::jitter::channel(2880);
                feed.metrics.device_latency_us.store(latency_us, Ordering::Relaxed);
                (String::new(), feed)
            })
            .collect();
        let buffers = Buffers(outputs, None);
        let mut smoothed = Vec::new();
        align(&mut smoothed, &buffers, 48_000);
        let sync: Vec<usize> = buffers.0.iter().map(|(_, feed)| feed.metrics.sync_frames.load(Ordering::Relaxed)).collect();
        // 40 and 35 ms more for the quicker two, none for the slowest (48 frames per ms), and
        // nothing yet for one that hasn't told its latency.
        assert_eq!(sync, [1920, 0, 1680, 0]);
    }

    #[test]
    fn stalled_telemetry_consumer_cannot_backpressure_reception() {
        let (publish, receiver) = std::sync::mpsc::sync_channel(2);
        let now = Instant::now();
        let mut monitor = Monitor { live: false, publish, sender: None, description: String::new(), packets: 0, bytes: 0, heard: now, last: now, rate: 48_000, sync: None };
        let mut buffers = Buffers(Vec::new(), None);
        let peak = AtomicU32::new(0);
        for i in 0..100 {
            monitor.arrived("127.0.0.1", 100, || "test".into());
            monitor.last = Instant::now() - SPECTRUM_EVERY;
            monitor.tick(&mut buffers, &peak);
            assert_eq!(monitor.packets, i + 1);
        }
        // Receiver has never read: only two snapshots are retained, updates after that drop.
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }
}
