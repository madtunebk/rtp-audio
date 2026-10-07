//! Playing the browser's microphone into the "RTP Audio Microphone" feed sink, through libpulse.

use std::error::Error;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use libpulse_binding as pa;
use pa::def::BufferAttr;
use pa::sample::{Format, Spec};
use pa::stream::{FlagSet, SeekMode, State, Stream};

use super::Event;
use super::pulse::Pulse;
use crate::sender::web::MicFeed;

/// What the browser sends: 48 kHz mono.
const RATE: u32 = 48_000;
/// How much the server keeps queued: 40 ms.
const TARGET_BYTES: u32 = RATE / 1000 * 40 * 2;

pub struct Playback<'a> {
    pulse: &'a Pulse,
    stream: Option<Box<Stream>>,
}

impl<'a> Playback<'a> {
    /// Play whatever the browser sends into `sink`, and silence when nothing comes. Problems once
    /// running arrive as `Event::Mic`.
    pub fn start(pulse: &'a Pulse, sink: &str, feed: Arc<MicFeed>, events: Sender<Event>) -> Result<Self, Box<dyn Error>> {
        let spec = Spec { format: Format::S16le, rate: RATE, channels: 1 };
        let attr = BufferAttr { maxlength: u32::MAX, tlength: TARGET_BYTES, prebuf: u32::MAX, minreq: u32::MAX, fragsize: u32::MAX };
        let wake = pulse.wake();
        let stream = pulse.locked(|mainloop, context| {
            let mut stream = Box::new(Stream::new(context, "RTP Audio Microphone", &spec, None).ok_or("could not create a playback stream")?);
            let raw: *mut Stream = &mut *stream;
            let mut samples = Vec::new();
            let write_events = events.clone();
            let mut failed = false;
            stream.set_write_callback(Some(Box::new(move |bytes| {
                // SAFETY: as in capture.rs, the callbacks are removed before the boxed stream is
                // dropped, and run with the main loop lock held.
                let stream = unsafe { &mut *raw };
                samples.resize(bytes / 2, 0i16);
                feed.take(&mut samples);
                let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                if let Err(err) = stream.write_copy(&data, 0, SeekMode::Relative)
                    && !failed
                {
                    failed = true;
                    let _ = write_events.send(Event::Mic(format!("writing to the sound server failed: {err}")));
                }
            })));
            stream.set_state_callback(Some(Box::new(move || {
                // SAFETY: as above.
                if matches!(unsafe { &*raw }.get_state(), State::Failed | State::Terminated) {
                    let _ = events.send(Event::Mic("its stream on the sound server stopped".into()));
                }
                wake.wake();
            })));
            let flags = FlagSet::ADJUST_LATENCY | FlagSet::DONT_MOVE;
            let mut result = stream.connect_playback(Some(sink), Some(&attr), flags, None, None).map_err(|err| format!("{err}"));
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
                    Err(format!("could not play the microphone into {sink}: {err}").into())
                }
            }
        });
        stream.map(|stream| Self { pulse, stream: Some(stream) })
    }
}

impl Drop for Playback<'_> {
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            self.pulse.locked(|_, _| {
                shut(&mut stream);
                drop(stream);
            });
        }
    }
}

fn shut(stream: &mut Stream) {
    stream.set_write_callback(None);
    stream.set_state_callback(None);
    let _ = stream.disconnect();
}
