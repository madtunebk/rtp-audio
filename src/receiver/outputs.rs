//! The sound outputs: listing them, picking them from --device (with delays), and playing on them.

use std::error::Error;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use super::jitter::{Jitter, Player};
use super::status::STATUS_WIDTH;

pub(super) fn device_name(device: &cpal::Device) -> String {
    device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "(unnamed)".into())
}

pub(super) fn device_id(device: &cpal::Device) -> String {
    device.id().map(|id| id.to_string()).unwrap_or_default()
}

pub(super) struct Outlet {
    name: String,
    id: String,
    device: cpal::Device,
}

/// The outputs, each real one once. ALSA lists every card port under several names (hw:,
/// plughw:, hdmi:, dmix:, sysdefault:, …): a card's hw: ports are kept, one per port even when
/// two share a name (two HDMI ports to the same model of monitor), and its other aliases are left
/// out. Outputs that aren't a card (PipeWire, default) and those of other systems are kept, each
/// ID once.
///
/// On Linux with a sound server, its outputs come first, then the cards' hw: ports opened directly:
/// the server only shows what a card's profile uses (one HDMI port at a time, none while the
/// monitor sleeps), and a port it isn't using can still be played on directly.
pub(super) fn outputs() -> Result<Vec<Outlet>, Box<dyn Error>> {
    let outlets = |host: &cpal::Host| -> Result<Vec<Outlet>, Box<dyn Error>> {
        Ok(host.output_devices()?.map(|device| Outlet { name: device_name(&device), id: device_id(&device), device }).collect())
    };
    let host = cpal::default_host();
    #[allow(unused_mut)]
    let mut all = outlets(&host)?;
    #[cfg(target_os = "linux")]
    if host.id() != cpal::HostId::Alsa
        && let Ok(alsa) = cpal::host_from_id(cpal::HostId::Alsa)
    {
        all.extend(outlets(&alsa)?.into_iter().filter(|o| alsa_port(&o.id).is_some_and(|port| port.plugin == "hw")));
    }
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
pub(super) const ALSA_HELPERS: &[&str] = &["lavrate", "samplerate", "speexrate", "speex", "upmix", "vdownmix", "jack", "oss"];

/// An ALSA card port, from an ID like alsa:hw:CARD=NVidia,DEV=3.
pub(super) struct AlsaPort {
    plugin: String,
    /// The card's name; a card given by number (CARD=1) is looked up, so both spellings match.
    card: String,
    dev: String,
}

pub(super) fn alsa_port(id: &str) -> Option<AlsaPort> {
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

/// `rtp-audio devices`: the sound outputs this computer can play on. With --json, for programs:
/// [{"name":"HDA NVidia, 2590G5","id":"alsa:hw:CARD=NVidia,DEV=3","default":false,"direct":true}]
/// where direct is a card opened directly (not through the sound server).
pub fn list_devices(json: bool) -> Result<(), Box<dyn Error>> {
    let default = cpal::default_host().default_output_device().map(|d| device_id(&d));
    let outputs = outputs()?;
    if json {
        use crate::json::string;
        let items: Vec<String> = outputs
            .iter()
            .map(|o| {
                let direct = alsa_port(&o.id).is_some_and(|port| port.plugin == "hw");
                let default = Some(&o.id) == default.as_ref();
                format!(r#"{{"name":{},"id":{},"default":{default},"direct":{direct}}}"#, string(&o.name), string(&o.id))
            })
            .collect();
        println!("[{}]", items.join(","));
        return Ok(());
    }
    let width = outputs.iter().map(|o| o.name.chars().count()).max().unwrap_or(0);
    println!("Sound outputs (play on one with: rtp-audio --device NAME_OR_ID):");
    let direct = |o: &Outlet| alsa_port(&o.id).is_some_and(|port| port.plugin == "hw");
    let server = outputs.iter().any(|o| !direct(o)) && outputs.iter().any(|o| o.id.starts_with("pulseaudio:"));
    let mut heading_shown = false;
    for outlet in &outputs {
        if server && direct(outlet) && !heading_shown {
            println!("\n Sound cards opened directly (while the sound server isn't using them):");
            heading_shown = true;
        }
        let mark = if Some(&outlet.id) == default.as_ref() { "*" } else { " " };
        println!(" {mark} {:width$}   {}", outlet.name, outlet.id);
    }
    println!("\n* = the default output");
    Ok(())
}

/// The output with this ID or name, else the only one whose name contains it; or the default.
pub(super) fn pick_device(wanted: Option<&str>) -> Result<cpal::Device, Box<dyn Error>> {
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
    let mut matches: Vec<usize> = (0..outputs.len()).filter(|&i| outputs[i].name.to_lowercase().contains(&lower)).collect();
    // Matching both a sound server output and a card opened directly: the server's (shared with
    // other programs) is the one meant; the direct one is reached by its ID.
    let through_server: Vec<usize> = matches.iter().copied().filter(|&i| alsa_port(&outputs[i].id).is_none_or(|port| port.plugin != "hw")).collect();
    if !through_server.is_empty() && through_server.len() < matches.len() {
        matches = through_server;
    }
    match matches.as_slice() {
        [i] => Ok(outputs.swap_remove(*i).device),
        [] => Err(format!("no sound output matches '{wanted}'; see `rtp-audio devices`").into()),
        // Every match has the same name: only the IDs tell them apart.
        _ if matches.iter().all(|&i| outputs[i].name == outputs[matches[0]].name) => Err(format!(
            "'{wanted}' matches several outputs called {}: choose one by its ID ({})",
            outputs[matches[0]].name,
            matches.iter().map(|&i| outputs[i].id.as_str()).collect::<Vec<_>>().join(", ")
        )
        .into()),
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
pub(super) fn pick_devices(wanted: &[String]) -> Result<Vec<(cpal::Device, Tuning)>, Box<dyn Error>> {
    if wanted.is_empty() {
        return Ok(vec![(pick_device(None)?, Tuning::default())]);
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
        // Each part may end with a delay and a volume for its output ("Bose+180ms@50%"); those of an
        // output whose name spans several parts are the last part's.
        let parts: Vec<(&str, Tuning)> = value.split(',').map(split_tuning).collect::<Result<_, _>>()?;
        let mut i = 0;
        while i < parts.len() {
            let joined = |j: usize| parts[i..j].iter().map(|(text, _)| *text).collect::<Vec<_>>().join(",");
            let longest = (i + 1..=parts.len()).rev().find_map(|j| exact(joined(j).trim()).map(|k| (j, k)));
            match longest {
                Some((j, k)) => {
                    devices.push((outputs[k].device.clone(), parts[j - 1].1));
                    i = j;
                }
                None => {
                    let (part, tuning) = (parts[i].0.trim(), parts[i].1);
                    if !part.is_empty() {
                        devices.push((pick_device(Some(part))?, tuning));
                    }
                    i += 1;
                }
            }
        }
    }
    // The same output named twice would play the sound twice, slightly apart.
    let mut seen = std::collections::HashSet::new();
    devices.retain(|(device, _)| seen.insert(device_id(device)));
    if devices.is_empty() {
        return Err("--device names no output; see `rtp-audio devices`".into());
    }
    Ok(devices)
}

/// The longest delay an output can be given, to line it up with slower ones.
pub(super) const MAX_DELAY_MS: u32 = 2_000;

/// What one output in the --device list asks for besides its name: a delay and a volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Tuning {
    pub(super) delay_ms: u32,
    /// Times the --volume: 1.0 is unchanged.
    pub(super) volume: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { delay_ms: 0, volume: 1.0 }
    }
}

/// The loudest an output can be made on its own, in percent (like --volume).
pub(super) const MAX_OUTPUT_VOLUME: f32 = 400.0;

/// One part of the --device list and what it asks for, at its end in either order: a delay
/// ("Bose+180ms") and a volume ("Bose@50%"). "Bose+180ms@50%" → ("Bose", 180 ms, 0.5).
pub(super) fn split_tuning(part: &str) -> Result<(&str, Tuning), String> {
    let mut tuning = Tuning::default();
    let (mut name, mut delay, mut volume) = (part, false, false);
    loop {
        let trimmed = name.trim_end();
        if !delay && let Some((rest, ms)) = suffix(trimmed, "ms", '+') {
            let ms: u32 = ms.parse().map_err(|_| format!("bad delay in '{}'", part.trim()))?;
            if ms > MAX_DELAY_MS {
                return Err(format!("the delay in '{}' must be at most {MAX_DELAY_MS} ms", part.trim()));
            }
            (tuning.delay_ms, name, delay) = (ms, rest, true);
        } else if !volume && let Some((rest, percent)) = suffix(trimmed, "%", '@') {
            let percent: f32 = percent.parse().map_err(|_| format!("bad volume in '{}'", part.trim()))?;
            if percent > MAX_OUTPUT_VOLUME {
                return Err(format!("the volume in '{}' must be at most {MAX_OUTPUT_VOLUME}%", part.trim()));
            }
            (tuning.volume, name, volume) = (percent / 100.0, rest, true);
        } else {
            return Ok((if delay || volume { trimmed } else { part }, tuning));
        }
    }
}

/// "NAME + 80 ms" with unit "ms" and mark '+' → ("NAME", "80"): a whole number after the mark.
fn suffix<'a>(text: &'a str, unit: &str, mark: char) -> Option<(&'a str, &'a str)> {
    let (rest, number) = text.strip_suffix(unit)?.rsplit_once(mark)?;
    let number = number.trim();
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit())).then(|| (rest.trim_end(), number))
}

/// How much a card opened directly (ALSA hw:) is asked for at a time: 20 ms. Left to itself,
/// ALSA can pick seconds, which the jitter buffer can't keep level with; 10 ms was too tight for a
/// card playing next to other outputs. Outputs that go through a sound server (PipeWire,
/// PulseAudio, the default) keep their own sizes: a short period runs them dry.
pub(super) const CARD_PERIOD_MS: u32 = 20;

/// The sample formats sound is played in, best first.
pub(super) const FORMATS: [SampleFormat; 5] = [SampleFormat::F32, SampleFormat::I32, SampleFormat::I16, SampleFormat::F64, SampleFormat::U16];

/// The device's own preferred configuration when its format is one we play, else the same rate
/// and channels in the best format it offers (a sound server may prefer 24-bit, which it would
/// convert from any other anyway).
pub(super) fn playable_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, Box<dyn Error>> {
    let preferred = device.default_output_config()?;
    if FORMATS.contains(&preferred.sample_format()) {
        return Ok(preferred);
    }
    let rate = preferred.sample_rate();
    let ranges: Vec<_> = device.supported_output_configs()?.collect();
    FORMATS
        .iter()
        .find_map(|&format| {
            ranges
                .iter()
                .filter(|range| range.sample_format() == format && range.min_sample_rate() <= rate && rate <= range.max_sample_rate())
                .max_by_key(|range| range.channels() == preferred.channels())
                .map(|range| range.with_sample_rate(rate))
        })
        .ok_or_else(|| format!("its sample format {} is not supported", preferred.sample_format()).into())
}

/// Start playing on `device`; returns the stream and a description of it.
pub(super) fn open_output(device: &cpal::Device, out: Output) -> Result<(cpal::Stream, String), Box<dyn Error>> {
    let supported = playable_config(device)?;
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

/// A short name for an output on the status line: the part in brackets at the end ("… Digital
/// Stereo (HDMI 2)" → "HDMI 2"), else the port after the card ("HDA NVidia, HDMI 3" → "HDMI 3"),
/// else the name; at most 24 characters.
pub(super) fn short_name(name: &str) -> String {
    let short = name
        .strip_suffix(')')
        .and_then(|rest| rest.rfind('(').map(|i| &rest[i + 1..]))
        .filter(|inside| !inside.is_empty())
        .or_else(|| name.rsplit_once(", ").map(|(_, port)| port))
        .unwrap_or(name);
    short.chars().take(24).collect()
}

/// How long after opening an output its errors are ignored: starting up, not a problem.
const STARTING: Duration = Duration::from_secs(1);

/// What the sound card callback needs.
pub(super) struct Output {
    pub(super) jitter: Arc<Mutex<Jitter>>,
    pub(super) peak: Arc<AtomicU32>,
    pub(super) volume: f32,
    pub(super) input_rate: u32,
}

pub(super) fn play<T>(device: &cpal::Device, config: &StreamConfig, out: Output) -> Result<cpal::Stream, Box<dyn Error>>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut player = Player::new(out.input_rate, config.sample_rate);
    let Output { jitter, peak, volume, .. } = out;
    let errors = Arc::clone(&jitter);
    let name = device_name(device);
    let mut shown = false;
    let opened = Instant::now();
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
        // on a line of its own, so a card that keeps hiccuping doesn't flood the terminal. A card
        // opened directly often runs dry once while starting, before the first sound reaches it:
        // harmless, so the first moments aren't reported.
        move |err| {
            if opened.elapsed() < STARTING {
                return;
            }
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
    #[test]
    fn delays_and_volumes_in_the_device_list() {
        use super::{Tuning, split_tuning};
        let tuned = |delay_ms, volume| Tuning { delay_ms, volume };
        assert_eq!(split_tuning("Bose+180ms"), Ok(("Bose", tuned(180, 1.0))));
        assert_eq!(split_tuning(" HDMI 2 + 80 ms"), Ok((" HDMI 2", tuned(80, 1.0))));
        assert_eq!(split_tuning("HDMI 2"), Ok(("HDMI 2", Tuning::default())));
        assert_eq!(split_tuning("Speakers + Mic"), Ok(("Speakers + Mic", Tuning::default()))); // a name with a plus
        assert!(split_tuning("Bose+5000ms").is_err());
        assert_eq!(split_tuning("Bose@50%"), Ok(("Bose", tuned(0, 0.5))));
        assert_eq!(split_tuning("Bose+180ms@50%"), Ok(("Bose", tuned(180, 0.5))));
        assert_eq!(split_tuning("Bose @ 50 % + 180 ms"), Ok(("Bose", tuned(180, 0.5))));
        assert_eq!(split_tuning("Speakers @ home"), Ok(("Speakers @ home", Tuning::default())));
        assert!(split_tuning("Bose@500%").is_err());
    }

    #[test]
    fn short_output_names() {
        assert_eq!(super::short_name("GA106 High Definition Audio Controller Digital Stereo (HDMI 2)"), "HDMI 2");
        assert_eq!(super::short_name("Bose Flex SoundLink"), "Bose Flex SoundLink");
        assert_eq!(super::short_name("HD-Audio Generic, ALC897 Analog"), "ALC897 Analog");
    }
}
