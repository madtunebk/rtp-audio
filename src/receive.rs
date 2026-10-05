use std::error::Error;
use std::io::{ErrorKind, IsTerminal, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use socket2::{Domain, Protocol, Socket, Type};

use crate::jitter::{Jitter, Player, Stats};
use crate::rtp;

pub struct Options {
    pub port: u16,
    pub latency_ms: u32,
    pub rate: u32,
    pub channels: usize,
    /// Output device name, or part of it; the default output if None.
    pub device: Option<String>,
    /// 1.0 is unchanged.
    pub volume: f32,
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

/// Receive RTP audio on a UDP port and play it on a sound card until killed.
pub fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let device = pick_device(options.device.as_deref())?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let target = (options.rate * options.latency_ms / 1000).max(1) as usize;
    let jitter = Arc::new(Mutex::new(Jitter::new(target)));
    // Loudest sample played since the status line last looked, as f32 bits.
    let peak = Arc::new(AtomicU32::new(0));
    let out = Output { jitter: Arc::clone(&jitter), peak: Arc::clone(&peak), volume: options.volume, input_rate: options.rate };

    let stream = match format {
        SampleFormat::F32 => play::<f32>(&device, &config, out),
        SampleFormat::I16 => play::<i16>(&device, &config, out),
        SampleFormat::U16 => play::<u16>(&device, &config, out),
        SampleFormat::I32 => play::<i32>(&device, &config, out),
        SampleFormat::F64 => play::<f64>(&device, &config, out),
        other => return Err(format!("sound card sample format {other} is not supported").into()),
    }?;
    stream.play()?;

    let socket = bind(options.port)?;
    println!(
        "Listening on UDP port {} ({} Hz, {} ch, {} ms buffer); sound card: {} ({} Hz, {} ch, {format})",
        options.port, options.rate, options.channels, options.latency_ms, device_name(&device), config.sample_rate, config.channels
    );
    if options.volume != 1.0 {
        println!("Volume: {:.0}%", options.volume * 100.0);
    }
    println!("Ctrl+C to quit.");

    // At a terminal, one status line updates in place; in a log, only events and problems.
    let live = std::io::stdout().is_terminal();
    socket.set_read_timeout(Some(STATUS_EVERY))?;
    let mut buf = [0u8; 65536];
    let mut sender: Option<SocketAddr> = None;
    let mut last_report = Instant::now();
    let mut reported = Stats::default();
    let (mut last_status, mut packets, mut shown) = (Instant::now(), 0u32, Stats::default());
    let (mut last_packet, mut silent_reported) = (Instant::now(), true);
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let Some(packet) = rtp::parse(&buf[..len]) else { continue };
                packets += 1;
                last_packet = Instant::now();
                silent_reported = false;
                if sender != Some(from) {
                    if live {
                        print!("\r{:width$}\r", "", width = STATUS_WIDTH);
                    }
                    println!("Receiving from {from}");
                    sender = Some(from);
                }
                let mut jitter = jitter.lock().unwrap();
                jitter.push(packet.sequence, packet.payload, options.channels);
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => return Err(err.into()),
        }
        let (stats, buffered) = {
            let jitter = jitter.lock().unwrap();
            (jitter.stats, jitter.buffered())
        };
        if live && last_status.elapsed() >= STATUS_EVERY {
            let seconds = last_status.elapsed().as_secs_f32();
            let level = f32::from_bits(peak.swap(0, Ordering::Relaxed));
            let waiting = last_packet.elapsed() > Duration::from_secs(1);
            print!("\r{}", status_line(sender, waiting, packets as f32 / seconds, buffered * 1000 / options.rate as usize, &stats, &shown, level));
            let _ = std::io::stdout().flush();
            (last_status, packets) = (Instant::now(), 0);
            if stats != shown && last_report.elapsed() >= REPORT_EVERY {
                (shown, last_report) = (stats, Instant::now());
            }
        } else if !live {
            if last_report.elapsed() >= REPORT_EVERY && stats != reported {
                report(&reported, &stats);
                reported = stats;
                last_report = Instant::now();
            }
            if !silent_reported && last_packet.elapsed() > Duration::from_secs(5) {
                println!("No sound arriving (sender stopped or paused)");
                silent_reported = true;
            }
        }
    }
}

const STATUS_EVERY: Duration = Duration::from_millis(250);
const STATUS_WIDTH: usize = 100;

/// The live status line: who, how much, how full the buffer is, problems, and a level meter.
fn status_line(sender: Option<SocketAddr>, waiting: bool, rate: f32, buffer_ms: usize, now: &Stats, since: &Stats, level: f32) -> String {
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
fn bind(port: u16) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(err) = socket.set_recv_buffer_size(1 << 20) {
        eprintln!("could not enlarge the receive buffer: {err}");
    }
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(socket.into())
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
