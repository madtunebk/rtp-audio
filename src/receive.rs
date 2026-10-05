use std::error::Error;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
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
}

/// Receive RTP audio on a UDP port and play it on the default sound card until killed.
pub fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let device = cpal::default_host().default_output_device().ok_or("no sound output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let target = (options.rate * options.latency_ms / 1000).max(1) as usize;
    let jitter = Arc::new(Mutex::new(Jitter::new(target)));

    let stream = match format {
        SampleFormat::F32 => play::<f32>(&device, &config, &jitter, options.rate),
        SampleFormat::I16 => play::<i16>(&device, &config, &jitter, options.rate),
        SampleFormat::U16 => play::<u16>(&device, &config, &jitter, options.rate),
        SampleFormat::I32 => play::<i32>(&device, &config, &jitter, options.rate),
        SampleFormat::F64 => play::<f64>(&device, &config, &jitter, options.rate),
        other => return Err(format!("sound card sample format {other} is not supported").into()),
    }?;
    stream.play()?;

    let socket = bind(options.port)?;
    println!(
        "Listening on UDP port {} ({} Hz, {} ch, {} ms buffer); sound card: {} Hz, {} ch, {format}",
        options.port, options.rate, options.channels, options.latency_ms, config.sample_rate, config.channels
    );
    println!("Ctrl+C to quit.");

    let mut buf = [0u8; 65536];
    let mut sender: Option<SocketAddr> = None;
    let mut last_report = Instant::now();
    let mut reported = Stats::default();
    loop {
        let (len, from) = socket.recv_from(&mut buf)?;
        let Some(packet) = rtp::parse(&buf[..len]) else { continue };
        if sender != Some(from) {
            println!("Receiving from {from}");
            sender = Some(from);
        }
        let stats = {
            let mut jitter = jitter.lock().unwrap();
            jitter.push(packet.sequence, packet.payload, options.channels);
            jitter.stats
        };
        if last_report.elapsed() >= REPORT_EVERY && stats != reported {
            report(&reported, &stats);
            reported = stats;
            last_report = Instant::now();
        }
    }
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

fn play<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    jitter: &Arc<Mutex<Jitter>>,
    input_rate: u32,
) -> Result<cpal::Stream, Box<dyn Error>>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut player = Player::new(input_rate, config.sample_rate);
    let jitter = jitter.clone();
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let mut jitter = jitter.lock().unwrap();
            player.update_speed(&jitter);
            for frame in data.chunks_mut(channels) {
                let [left, right] = player.next_frame(&mut jitter);
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
        },
        |err| eprintln!("sound card: {err}"),
        None,
    )?;
    Ok(stream)
}
