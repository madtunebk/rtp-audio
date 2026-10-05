//! `rtp-audio send`: the sender's lifecycle, from the lock to switching sound back.

use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use std::sync::Arc;

use super::capture::{self, Capture};
use super::playback::Playback;
use super::pulse::Pulse;
use super::routing::{self, DISPLAY_NAME, MIC_NAME, Routing};
use super::{Event, fail_point, lock};
use crate::transport::{AudioSink, Encoding, RtpSender};
use crate::web::{self, MicFeed};

/// Ctrl+C (or SIGTERM) arrived before streaming started.
#[derive(Debug)]
struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl Error for Interrupted {}

/// Send to a receiver at `destination`, to browsers through a server on `web`, or both. With
/// `mic`, browsers can also send their microphone, which apps here hear as "RTP Audio Microphone".
pub fn run(
    destinations: &[SocketAddr],
    encoding: Encoding,
    source: Option<&str>,
    web: Option<SocketAddr>,
    mic: bool,
) -> Result<(), Box<dyn Error>> {
    let mic = mic.then(|| Arc::new(MicFeed::default()));
    // Fail on a bad network or a busy port before touching any sound settings.
    let mut sinks: Vec<Box<dyn AudioSink>> = Vec::new();
    let mut targets = Vec::new();
    for &destination in destinations {
        let rtp = RtpSender::connect(destination, capture::RATE, capture::CHANNELS.into(), encoding.clone())
            .map_err(|err| format!("cannot send to {destination}: {err}"))?;
        targets.push(format!("{destination} ({})", rtp.describe()));
        sinks.push(Box::new(rtp));
    }
    if let Some(address) = web {
        sinks.push(Box::new(web::start(address, mic.clone())?));
        targets.push(format!("browsers (http://{address})"));
        if !address.ip().is_loopback() {
            eprintln!(
                "Warning: anyone who can reach {address} can listen, without logging in.\n  \
                 Use 127.0.0.1 and put it behind NGINX's HTTPS and login (see docs/web.md)."
            );
        }
    }
    let target = targets.join(" and ");
    // From here on, Ctrl+C and SIGTERM only ask for an orderly stop.
    let (events, stop) = mpsc::channel();
    watch_signals(events.clone())?;
    let _lock = lock::acquire()?;
    let pulse = Pulse::connect()?;
    let state = routing::state_path(&lock::runtime_dir()?, &pulse.server());
    routing::recover(&pulse, &state)?;
    check_interrupted(&stop)?;

    let mut routing = Routing::new(&pulse, state);
    let result = (|| {
        // The microphone first: with it, a call can start before the sound does.
        let _mic = mic.map(|feed| start_mic(&pulse, &mut routing, feed)).transpose()?;
        check_interrupted(&stop)?;
        if source.is_some() && routing.mic_feed_is_default()? {
            eprintln!(
                "Warning: there is no other output, so sounds played here go into \"{MIC_NAME}\" too. \
                 Without --source, \"{DISPLAY_NAME}\" becomes the default output instead."
            );
        }
        match source {
            Some(source) => send_source(&pulse, source, sinks, events, &stop, &target),
            None => send_all(&pulse, &mut routing, sinks, events, &stop, &target),
        }
    })();
    // Runs whatever happened above, including a failure halfway through starting.
    routing.restore();
    match result {
        Err(err) if err.is::<Interrupted>() => Ok(()),
        result => result,
    }
}

/// The browser microphone: create "RTP Audio Microphone" and play what browsers send into it.
fn start_mic<'a>(pulse: &'a Pulse, routing: &mut Routing, feed: Arc<MicFeed>) -> Result<Playback<'a>, Box<dyn Error>> {
    let sink = routing.create_mic()?;
    fail_point("mic")?;
    let playback = Playback::start(pulse, &sink, feed)?;
    eprintln!("Browser microphone: \"{MIC_NAME}\" is now the default input.");
    Ok(playback)
}

/// Explicit source mode: record one source; no output settings change.
fn send_source(
    pulse: &Pulse,
    wanted: &str,
    sinks: Vec<Box<dyn AudioSink>>,
    events: Sender<Event>,
    stop: &Receiver<Event>,
    target: &str,
) -> Result<(), Box<dyn Error>> {
    let sources = pulse.sources()?;
    let source = sources
        .iter()
        .find(|source| source.name == wanted)
        .or_else(|| wanted.parse().ok().and_then(|id: u32| sources.iter().find(|source| source.index == id)))
        .ok_or_else(|| format!("unknown source '{wanted}'; see `rtp-audio sources` for the names and IDs"))?;
    let _capture = Capture::start(pulse, &source.name, sinks, events)?;
    eprintln!("Sending {} ({}) to {target}. {}", source.description, source.name, stop_hint());
    wait(pulse, stop, target)
}

/// Automatic mode: everything this computer plays goes to the "RTP Audio" output, which we send.
fn send_all(
    pulse: &Pulse,
    routing: &mut Routing,
    sinks: Vec<Box<dyn AudioSink>>,
    events: Sender<Event>,
    stop: &Receiver<Event>,
    target: &str,
) -> Result<(), Box<dyn Error>> {
    (|| {
        let monitor = routing.create_sink()?;
        fail_point("sink")?;
        check_interrupted(stop)?;
        routing.make_default()?;
        fail_point("default")?;
        check_interrupted(stop)?;
        let moved = routing.move_streams()?;
        fail_point("move")?;
        check_interrupted(stop)?;
        let _capture = Capture::start(pulse, &monitor, sinks, events)?;
        fail_point("capture")?;
        eprintln!(
            "Sending all sound to {target}: \"{DISPLAY_NAME}\" is now the default output{}.\n{}",
            match moved {
                0 => String::new(),
                1 => " (1 playing stream moved to it)".into(),
                n => format!(" ({n} playing streams moved to it)"),
            },
            stop_hint()
        );
        wait(pulse, stop, target)
    })()
}

/// How to stop: Ctrl+C in a terminal, or through the service.
fn stop_hint() -> &'static str {
    if super::service::is_service_process(std::process::id()) {
        "Running as the rtp-audio service: `rtp-audio service stop` stops it and switches sound back."
    } else {
        "Ctrl+C to stop and switch sound back."
    }
}

/// Stream until Ctrl+C, SIGTERM or an error.
fn wait(pulse: &Pulse, stop: &Receiver<Event>, target: &str) -> Result<(), Box<dyn Error>> {
    match stop.recv() {
        Ok(Event::Signal) | Err(_) => {
            eprintln!("Stopping");
            Ok(())
        }
        Ok(Event::Network(err)) => Err(format!("sending to {target} failed: {err}").into()),
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
