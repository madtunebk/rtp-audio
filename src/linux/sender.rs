//! `rtp-audio send`: the sender's lifecycle, from the lock to switching sound back.

use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::capture::{self, Capture};
use super::pulse::Pulse;
use super::routing::{self, DISPLAY_NAME, Routing};
use super::{Event, fail_point, lock};
use crate::transport::RtpSender;

/// Ctrl+C (or SIGTERM) arrived before streaming started.
#[derive(Debug)]
struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl Error for Interrupted {}

pub fn run(destination: SocketAddr, source: Option<&str>) -> Result<(), Box<dyn Error>> {
    // Fail on a bad network before touching any sound settings.
    let rtp = RtpSender::connect(destination, capture::RATE, capture::CHANNELS.into())
        .map_err(|err| format!("cannot send to {destination}: {err}"))?;
    // From here on, Ctrl+C and SIGTERM only ask for an orderly stop.
    let (events, stop) = mpsc::channel();
    watch_signals(events.clone())?;
    let _lock = lock::acquire()?;
    let pulse = Pulse::connect()?;
    let state = routing::state_path(&lock::runtime_dir()?, &pulse.server());
    routing::recover(&pulse, &state)?;
    check_interrupted(&stop)?;

    let result = match source {
        Some(source) => send_source(&pulse, source, rtp, events, &stop, destination),
        None => send_all(&pulse, state, rtp, events, &stop, destination),
    };
    match result {
        Err(err) if err.is::<Interrupted>() => Ok(()),
        result => result,
    }
}

/// Explicit source mode: record one source; no sound settings change.
fn send_source(
    pulse: &Pulse,
    wanted: &str,
    rtp: RtpSender,
    events: Sender<Event>,
    stop: &Receiver<Event>,
    destination: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let sources = pulse.sources()?;
    let source = sources
        .iter()
        .find(|source| source.name == wanted)
        .or_else(|| wanted.parse().ok().and_then(|id: u32| sources.iter().find(|source| source.index == id)))
        .ok_or_else(|| format!("unknown source '{wanted}'; see `rtp-audio sources` for the names and IDs"))?;
    let _capture = Capture::start(pulse, &source.name, rtp, events)?;
    eprintln!("Sending {} ({}) to {destination}. Ctrl+C to stop.", source.description, source.name);
    wait(pulse, stop, destination)
}

/// Automatic mode: everything this computer plays goes to the "RTP Audio" output, which we send.
fn send_all(
    pulse: &Pulse,
    state: std::path::PathBuf,
    rtp: RtpSender,
    events: Sender<Event>,
    stop: &Receiver<Event>,
    destination: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let mut routing = Routing::new(pulse, state);
    let result = (|| {
        let monitor = routing.create_sink()?;
        fail_point("sink")?;
        check_interrupted(stop)?;
        routing.make_default()?;
        fail_point("default")?;
        check_interrupted(stop)?;
        let moved = routing.move_streams()?;
        fail_point("move")?;
        check_interrupted(stop)?;
        let _capture = Capture::start(pulse, &monitor, rtp, events)?;
        fail_point("capture")?;
        eprintln!(
            "Sending all sound to {destination}: \"{DISPLAY_NAME}\" is now the default output{}.\n\
             Ctrl+C to stop and switch sound back.",
            match moved {
                0 => String::new(),
                1 => " (1 playing stream moved to it)".into(),
                n => format!(" ({n} playing streams moved to it)"),
            }
        );
        wait(pulse, stop, destination)
    })();
    // Runs whatever happened above, including a failure halfway through starting.
    routing.restore();
    result
}

/// Stream until Ctrl+C, SIGTERM or an error.
fn wait(pulse: &Pulse, stop: &Receiver<Event>, destination: SocketAddr) -> Result<(), Box<dyn Error>> {
    match stop.recv() {
        Ok(Event::Signal) | Err(_) => {
            eprintln!("Stopping");
            Ok(())
        }
        Ok(Event::Network(err)) => Err(format!("sending to {destination} failed: {err}").into()),
        Ok(Event::Capture(_)) if !pulse.is_connected() => Err("the connection to the sound server was lost".into()),
        Ok(Event::Capture(err)) => Err(format!("{err} (was the source removed?)").into()),
    }
}

fn check_interrupted(stop: &Receiver<Event>) -> Result<(), Box<dyn Error>> {
    match stop.try_recv() {
        Ok(Event::Signal) => {
            eprintln!("Interrupted before sending started");
            Err(Interrupted.into())
        }
        _ => Ok(()),
    }
}

/// Turn SIGINT, SIGTERM and SIGHUP into an `Event::Signal`. If switching back hangs, a third
/// signal quits at once (the next start then finishes switching back).
fn watch_signals(events: Sender<Event>) -> Result<(), Box<dyn Error>> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    std::thread::spawn(move || {
        for (count, _) in signals.forever().enumerate() {
            match count {
                0 => {
                    let _ = events.send(Event::Signal);
                }
                1 => eprintln!("Still switching sound back; press Ctrl+C again to quit now"),
                _ => std::process::exit(130),
            }
        }
    });
    Ok(())
}
