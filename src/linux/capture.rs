//! Recording a source through libpulse and handing the audio straight to the senders.

use std::error::Error;
use std::sync::mpsc::Sender;

use libpulse_binding as pa;
use pa::def::BufferAttr;
use pa::sample::{Format, Spec};
use pa::stream::{FlagSet, PeekResult, State, Stream};

use super::Event;
use super::pulse::Pulse;
use crate::transport::AudioSink;

/// What the receiver expects: 48 kHz stereo, big-endian 16-bit (RTP L16).
pub const RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
/// How much audio the server hands over at a time: 20 ms, small enough for low latency.
const FRAGMENT_BYTES: u32 = RATE / 1000 * 20 * CHANNELS as u32 * 2;
/// The longest gap ("hole") in the recording turned into silence: 2 s.
const MAX_HOLE_BYTES: usize = RATE as usize * 2 * CHANNELS as usize * 2;

pub struct Capture<'a> {
    pulse: &'a Pulse,
    stream: Option<Box<Stream>>,
}

impl<'a> Capture<'a> {
    /// Start recording `source`, handing everything to each sink. Problems once running arrive
    /// as `Event`s.
    pub fn start(
        pulse: &'a Pulse,
        source: &str,
        mut sinks: Vec<Box<dyn AudioSink>>,
        events: Sender<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let spec = Spec { format: Format::S16be, rate: RATE, channels: CHANNELS };
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: FRAGMENT_BYTES,
        };
        let wake = pulse.wake();
        let stream = pulse.locked(|mainloop, context| {
            let mut stream = Box::new(Stream::new(context, "RTP Audio", &spec, None).ok_or("could not create a capture stream")?);
            let raw: *mut Stream = &mut *stream;

            let read_events = events.clone();
            let mut failed = false;
            let silence = vec![0u8; FRAGMENT_BYTES as usize];
            stream.set_read_callback(Some(Box::new(move |_| {
                // SAFETY: the callbacks are removed before the boxed stream is dropped, and run
                // with the main loop lock held, like every other use of the stream.
                let stream = unsafe { &mut *raw };
                loop {
                    match stream.peek() {
                        Ok(PeekResult::Empty) => break,
                        // A gap the server had nothing for: silence as long, so what follows
                        // keeps its place in time.
                        Ok(PeekResult::Hole(bytes)) => {
                            let mut left = bytes.min(MAX_HOLE_BYTES) / 4 * 4;
                            while left > 0 {
                                let chunk = left.min(silence.len());
                                push(&mut sinks, &silence[..chunk], &mut failed, &read_events);
                                left -= chunk;
                            }
                        }
                        Ok(PeekResult::Data(data)) => push(&mut sinks, data, &mut failed, &read_events),
                        Err(err) => {
                            let _ = read_events.send(Event::Capture(format!("reading audio failed: {err}")));
                            break;
                        }
                    }
                    if let Err(err) = stream.discard() {
                        let _ = read_events.send(Event::Capture(format!("reading audio failed: {err}")));
                        break;
                    }
                }
            })));
            stream.set_state_callback(Some(Box::new(move || {
                // SAFETY: as above.
                if unsafe { &*raw }.get_state() == State::Failed {
                    let _ = events.send(Event::Capture("the capture stream stopped".into()));
                }
                wake.wake();
            })));

            let flags = FlagSet::ADJUST_LATENCY | FlagSet::DONT_MOVE;
            let mut result = stream.connect_record(Some(source), Some(&attr), flags).map_err(|err| format!("{err}"));
            while result.is_ok() {
                match stream.get_state() {
                    State::Ready => break,
                    State::Failed | State::Terminated => result = Err(format!("{}", context.errno())),
                    _ => mainloop.wait(),
                }
            }
            match result {
                Ok(()) => Ok(stream),
                Err(err) => {
                    shut(&mut stream);
                    Err(format!("could not capture from {source}: {err}").into())
                }
            }
        });
        stream.map(|stream| Self { pulse, stream: Some(stream) })
    }
}

/// Hand `data` to every sink; the first failure ends sending (reported once).
fn push(sinks: &mut [Box<dyn AudioSink>], data: &[u8], failed: &mut bool, events: &Sender<Event>) {
    for sink in sinks.iter_mut() {
        if !*failed && let Err(err) = sink.push(data) {
            *failed = true;
            let _ = events.send(Event::Network(err));
        }
    }
}

impl Drop for Capture<'_> {
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            self.pulse.locked(|_, _| {
                shut(&mut stream);
                drop(stream);
            });
        }
    }
}

/// Remove the callbacks (which point at the stream) and disconnect. Main loop lock held.
fn shut(stream: &mut Stream) {
    stream.set_read_callback(None);
    stream.set_state_callback(None);
    let _ = stream.disconnect();
}
