//! What the receiver tells: the live status line at a terminal, or reports in a log.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::Buffers;
use super::jitter::Stats;

/// The live status line at a terminal, or occasional reports in a log, for either source.
pub(super) struct Monitor {
    pub(super) live: bool,
    /// A JSON status line every JSON_EVERY instead of the status line or the reports, naming the
    /// outputs by these IDs.
    json: Option<Vec<String>>,
    rate: u32,
    sender: Option<String>,
    packets: u32,
    last_status: Instant,
    /// Per output, the totals the status line and the reports last counted from.
    shown: Vec<Stats>,
    last_spectrum: Instant,
    last_report: Instant,
    reported: Vec<Stats>,
    last_packet: Instant,
    silent_reported: bool,
}

impl Monitor {
    pub(super) fn new(rate: u32, json: Option<Vec<String>>) -> Self {
        let now = Instant::now();
        Monitor {
            // At a terminal, one status line updates in place; in a log, only events and problems.
            live: json.is_none() && std::io::stdout().is_terminal(),
            json,
            rate,
            sender: None,
            packets: 0,
            last_status: now,
            shown: Vec::new(),
            last_spectrum: now,
            last_report: now,
            reported: Vec::new(),
            last_packet: now,
            silent_reported: true,
        }
    }

    /// Sound arrived from `from`; when that's a new sender, says so with `describe()`.
    pub(super) fn arrived(&mut self, from: &str, describe: impl FnOnce() -> String) {
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
    pub(super) fn forget_sender(&mut self) {
        self.sender = None;
    }

    pub(super) fn tick(&mut self, jitter: &Buffers, peak: &AtomicU32) {
        let (stats, buffered) = jitter.state();
        self.shown.resize(stats.len(), Stats::default());
        self.reported.resize(stats.len(), Stats::default());
        let labels = jitter.labels();
        if let Some(ids) = &self.json {
            // While sound arrives, its spectrum 20 times a second: {"spectrum":[0.12,…]}.
            if let Some(spectrum) = &jitter.1
                && self.last_spectrum.elapsed() >= SPECTRUM_EVERY
                && self.last_packet.elapsed() < SPECTRUM_EVERY
            {
                let bands = spectrum.lock().unwrap().bands();
                println!("{{\"spectrum\":[{}]}}", bands.iter().map(|b| format!("{b:.2}")).collect::<Vec<_>>().join(","));
                self.last_spectrum = Instant::now();
            }
            if self.last_status.elapsed() >= JSON_EVERY {
                let seconds = self.last_status.elapsed().as_secs_f32();
                let level = f32::from_bits(peak.swap(0, Ordering::Relaxed));
                let waiting = self.last_packet.elapsed() > Duration::from_secs(1);
                let rate = self.packets as f32 / seconds;
                println!("{}", json_status(self.sender.as_deref(), waiting, rate, buffered * 1000 / self.rate as usize, &labels, ids, &stats, level));
                (self.last_status, self.packets) = (Instant::now(), 0);
            }
        } else if self.live && self.last_status.elapsed() >= STATUS_EVERY {
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

pub(super) const STATUS_EVERY: Duration = Duration::from_millis(250);

/// How often --json prints the status, and the spectrum.
const JSON_EVERY: Duration = Duration::from_millis(500);
const SPECTRUM_EVERY: Duration = Duration::from_millis(50);

/// The status for a program: who sends, packets per second, the emptiest buffer, the level (dB,
/// null when silent), the network problems and each output's playing problems (by its ID from
/// `devices --json`), as totals since the start, on one line:
/// {"status":{"sender":"192.168.1.5:40000","waiting":false,"packets":50,"buffer_ms":60,"level_db":-12.3,
///  "lost":0,"late":0,"outputs":[{"name":"HDMI 2","id":"alsa:hw:…","dropouts":0,"skips":0,"card":0}]}}
#[allow(clippy::too_many_arguments)]
fn json_status(sender: Option<&str>, waiting: bool, rate: f32, buffer_ms: usize, labels: &[&str], ids: &[String], stats: &[Stats], level: f32) -> String {
    use crate::json::string;
    let sender = sender.map_or("null".to_string(), string);
    let level = if level > 1e-4 { format!("{:.1}", 20.0 * level.log10()) } else { "null".to_string() };
    let (lost, late) = stats.first().map_or((0, 0), |s| (s.lost, s.late));
    let outputs: Vec<String> = labels
        .iter()
        .zip(ids)
        .zip(stats)
        .map(|((name, id), s)| {
            format!(r#"{{"name":{},"id":{},"dropouts":{},"skips":{},"card":{}}}"#, string(name), string(id), s.underruns, s.trimmed, s.card)
        })
        .collect();
    format!(
        r#"{{"status":{{"sender":{sender},"waiting":{waiting},"packets":{rate:.0},"buffer_ms":{buffer_ms},"level_db":{level},"lost":{lost},"late":{late},"outputs":[{}]}}}}"#,
        outputs.join(",")
    )
}

pub(super) const STATUS_WIDTH: usize = 100;

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
        self.labels.iter().zip(self.now.iter().zip(self.since)).map(|(label, (now, since))| {
            (*label, now.underruns - since.underruns, now.trimmed - since.trimmed, now.card - since.card)
        })
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

/// The live status line: who, how much, how full the buffer is, problems (and on which output,
/// with several), and a level meter.
pub(super) fn status_line(sender: Option<&str>, waiting: bool, rate: f32, buffer_ms: usize, problems: Problems, level: f32) -> String {
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
