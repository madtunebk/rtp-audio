//! Sound for web browsers: the captured audio as Opus over a WebSocket, plus the small player
//! script that plays it, on one local port. Meant to sit behind NGINX, which adds HTTPS and the
//! login (see docs/web.md):
//!
//!   GET /player.js   the player: adds a sound button to the page that loads it
//!   GET /ws          the WebSocket: one text message describing the stream, then one binary
//!                    message per 20 ms frame (Opus, or s16le PCM with ?codec=pcm)
//!   GET /config.json what the player may offer: {"mic": true} with `send --web … --mic`
//!   GET /mic         the WebSocket the browser sends its microphone on (48 kHz mono, 20 ms
//!                    Opus frames, or s16le PCM with ?codec=pcm); one browser at a time

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Bytes, Message};

use crate::net::transport::AudioSink;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms frames: Opus's usual size, and what the player expects.
const FRAME_SAMPLES: usize = RATE as usize / 50 * CHANNELS;
/// Frames queued per listener (0.5 s); beyond that a slow listener misses frames instead of
/// slowing down everyone else.
const QUEUE: usize = 25;
/// Connections served at once (listeners, microphone and plain requests): each has a thread.
const MAX_CONNECTIONS: usize = 32;
/// A listener that takes longer than this to accept a frame is gone or hopelessly behind.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// With no sound to send, ping this often: proxies keep the connection, gone browsers show up.
const PING_EVERY: Duration = Duration::from_secs(1);
/// A microphone sends a frame every 20 ms; this long without anything means the browser is gone.
const MIC_IDLE: Duration = Duration::from_secs(10);
/// The largest WebSocket message taken from a browser: a 20 ms microphone frame is under 2 KB.
const MAX_MESSAGE: usize = 16 * 1024;
const PLAYER: &str = include_str!("player.js");

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    Opus,
    Pcm,
}

struct Listener {
    codec: Codec,
    frames: SyncSender<Bytes>,
}

/// The browsers listening right now.
#[derive(Default)]
struct Hub {
    listeners: Mutex<Vec<Listener>>,
}

impl Hub {
    fn join(&self, codec: Codec) -> Receiver<Bytes> {
        let (frames, rx) = mpsc::sync_channel(QUEUE);
        self.listeners.lock().unwrap().push(Listener { codec, frames });
        rx
    }

    fn wants(&self, codec: Codec) -> bool {
        self.listeners.lock().unwrap().iter().any(|l| l.codec == codec)
    }

    fn is_empty(&self) -> bool {
        self.listeners.lock().unwrap().is_empty()
    }

    /// Hand a frame to every listener of `codec`, forgetting those that left.
    fn send(&self, codec: Codec, frame: &Bytes) {
        self.listeners.lock().unwrap().retain(|l| {
            l.codec != codec || !matches!(l.frames.try_send(frame.clone()), Err(TrySendError::Disconnected(_)))
        });
    }
}

/// The browser's microphone, between the WebSocket and the sound server: 48 kHz mono samples.
#[derive(Default)]
pub struct MicFeed {
    queue: Mutex<MicQueue>,
    busy: AtomicBool,
}

#[derive(Default)]
struct MicQueue {
    samples: VecDeque<i16>,
    playing: bool,
}

/// Wait for 40 ms before playing (frames come in bursts over TCP); keep at most 250 ms so the
/// microphone never lags behind.
const MIC_START: usize = 1920;
const MIC_MAX: usize = 12_000;

impl MicFeed {
    fn push(&self, samples: &[i16]) {
        // Only the newest MIC_MAX samples can stay: don't copy more than that in.
        let samples = &samples[samples.len().saturating_sub(MIC_MAX)..];
        let mut queue = self.queue.lock().unwrap();
        queue.samples.extend(samples);
        let excess = queue.samples.len().saturating_sub(MIC_MAX);
        queue.samples.drain(..excess);
    }

    /// Fill `out` with what has arrived, silence for the rest.
    pub fn take(&self, out: &mut [i16]) {
        let mut queue = self.queue.lock().unwrap();
        if !queue.playing && queue.samples.len() < MIC_START {
            out.fill(0);
            return;
        }
        queue.playing = true;
        for sample in out.iter_mut() {
            *sample = queue.samples.pop_front().unwrap_or_else(|| {
                queue.playing = false;
                0
            });
        }
    }

    fn clear(&self) {
        *self.queue.lock().unwrap() = MicQueue::default();
    }
}

/// Encodes the captured sound once and hands it to every browser listening.
pub struct WebSink {
    hub: Arc<Hub>,
    encoder: opus::Encoder,
    frame: Vec<i16>,
    packet: Vec<u8>,
}

/// Start serving browsers on `address`; with `mic`, also take a browser's microphone. Returns
/// the sink to feed captured sound into.
pub fn start(address: SocketAddr, mic: Option<Arc<MicFeed>>, kbps: u32) -> Result<WebSink, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(address).map_err(|err| format!("cannot listen on {address}: {err}"))?;
    let hub = Arc::new(Hub::default());
    let server_hub = Arc::clone(&hub);
    let connections = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            if connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let count = Count::new(&connections);
            let hub = Arc::clone(&server_hub);
            let mic = mic.clone();
            std::thread::spawn(move || {
                let _count = count;
                if let Err(err) = serve(stream, &hub, mic.as_deref()) {
                    eprintln!("Web listener: {err}");
                }
            });
        }
    });
    let encoder = crate::net::transport::opus_encoder(RATE, opus::Channels::Stereo, kbps)?;
    Ok(WebSink { hub, encoder, frame: Vec::with_capacity(FRAME_SAMPLES), packet: vec![0; 4000] })
}

impl AudioSink for WebSink {
    fn push(&mut self, pcm: &[u8]) -> io::Result<()> {
        // Nobody listening: nothing to encode.
        if self.hub.is_empty() {
            self.frame.clear();
            return Ok(());
        }
        for sample in pcm.chunks_exact(2) {
            self.frame.push(i16::from_be_bytes([sample[0], sample[1]]));
            if self.frame.len() == FRAME_SAMPLES {
                self.send_frame();
                self.frame.clear();
            }
        }
        Ok(())
    }
}

impl WebSink {
    fn send_frame(&mut self) {
        if self.hub.wants(Codec::Opus) {
            match self.encoder.encode(&self.frame, &mut self.packet) {
                Ok(len) => self.hub.send(Codec::Opus, &Bytes::copy_from_slice(&self.packet[..len])),
                Err(err) => eprintln!("Opus encoding failed: {err}"),
            }
        }
        if self.hub.wants(Codec::Pcm) {
            let pcm: Vec<u8> = self.frame.iter().flat_map(|s| s.to_le_bytes()).collect();
            self.hub.send(Codec::Pcm, &Bytes::from(pcm));
        }
    }
}

/// One connection: a WebSocket listener, or a plain request for the player.
fn serve(mut stream: TcpStream, hub: &Hub, mic: Option<&MicFeed>) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let head = peek_head(&stream)?;
    let request_line = head.lines().next().unwrap_or_default();
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let upgrade = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("upgrade:") && l.contains("websocket")
    });

    if upgrade && (path == "/ws" || path.starts_with("/ws?")) {
        let codec = if path.contains("codec=pcm") { Codec::Pcm } else { Codec::Opus };
        // Frames are small and every 20 ms: send each at once rather than gathering them.
        stream.set_nodelay(true)?;
        // A few seconds of sound at most waiting in the kernel: a browser that stops reading then
        // fills it soon, and the write timeout notices, instead of after megabytes.
        let _ = socket2::SockRef::from(&stream).set_send_buffer_size(64 * 1024);
        let mut socket = tungstenite::accept_with_config(stream, Some(small_messages()))?;
        // Reads only look for what the browser sends back (Close, Ping), never wait for it.
        socket.get_mut().set_read_timeout(Some(Duration::from_millis(1)))?;
        socket.get_mut().set_write_timeout(Some(WRITE_TIMEOUT))?;
        let name = if codec == Codec::Opus { "opus" } else { "pcm" };
        socket.send(Message::text(format!(
            r#"{{"codec":"{name}","sampleRate":{RATE},"channels":{CHANNELS},"frameMs":20}}"#
        )))?;
        let frames = hub.join(codec);
        // Ends when the browser closes or goes away (a send fails or times out), or the sender stops.
        loop {
            let sent = match frames.recv_timeout(PING_EVERY) {
                Ok(frame) => socket.send(Message::Binary(frame)),
                Err(RecvTimeoutError::Timeout) => socket.send(Message::Ping(Bytes::new())),
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if sent.is_err() || browser_closed(&mut socket) {
                break;
            }
        }
        return Ok(());
    }

    if upgrade && (path == "/mic" || path.starts_with("/mic?"))
        && let Some(mic) = mic
    {
        {
            let pcm = path.contains("codec=pcm");
            let mut socket = tungstenite::accept_with_config(stream, Some(small_messages()))?;
            socket.get_mut().set_read_timeout(Some(MIC_IDLE))?;
            socket.get_mut().set_write_timeout(Some(WRITE_TIMEOUT))?;
            if mic.busy.swap(true, Ordering::SeqCst) {
                let _ = socket.close(Some(tungstenite::protocol::CloseFrame {
                    code: tungstenite::protocol::frame::coding::CloseCode::Policy,
                    reason: "another browser is using the microphone".into(),
                }));
                let _ = socket.flush();
                return Ok(());
            }
            // Free the microphone however this ends.
            let _slot = MicSlot(mic);
            return receive_mic(&mut socket, mic, pcm);
        }
    }

    // Consume the request we only peeked at, then answer it.
    let mut discard = vec![0; head.len()];
    stream.read_exact(&mut discard)?;
    let config = format!(r#"{{"mic":{}}}"#, mic.is_some());
    let (status, kind, body) = match path.split('?').next().unwrap_or("/") {
        "/player.js" => ("200 OK", "text/javascript; charset=utf-8", PLAYER),
        "/config.json" => ("200 OK", "application/json", config.as_str()),
        _ => ("404 Not Found", "text/plain; charset=utf-8", "not found\n"),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

/// Counts a connection while it lives.
struct Count(Arc<AtomicUsize>);

impl Count {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Count(Arc::clone(count))
    }
}

impl Drop for Count {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The microphone in use by one browser: free again when this is dropped.
struct MicSlot<'a>(&'a MicFeed);

impl Drop for MicSlot<'_> {
    fn drop(&mut self) {
        self.0.clear();
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

/// Browsers only send small messages: a big one is refused before it takes memory.
fn small_messages() -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE))
}

/// Read what a listening browser sent back, without waiting: true once it closed or went away.
/// A Ping gets its Pong with the next frame sent.
fn browser_closed(socket: &mut tungstenite::WebSocket<TcpStream>) -> bool {
    loop {
        match socket.read() {
            Ok(Message::Close(_)) => return true,
            Ok(_) => {}
            Err(tungstenite::Error::Io(err)) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return false;
            }
            Err(_) => return true,
        }
    }
}

/// Feed one browser's microphone into `mic` until it disconnects.
fn receive_mic(socket: &mut tungstenite::WebSocket<TcpStream>, mic: &MicFeed, pcm: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut decoder = opus::Decoder::new(RATE, opus::Channels::Mono)?;
    let mut samples = vec![0i16; 5760];
    eprintln!("Browser microphone connected");
    loop {
        match socket.read() {
            Ok(Message::Binary(data)) if pcm => {
                if data.len() % 2 != 0 {
                    eprintln!("Browser microphone: ignoring a PCM frame of odd length");
                    continue;
                }
                let frame: Vec<i16> = data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                mic.push(&frame);
            }
            Ok(Message::Binary(data)) => match decoder.decode(&data, &mut samples, false) {
                Ok(n) => mic.push(&samples[..n]),
                Err(err) => eprintln!("Browser microphone: bad Opus frame ({err})"),
            },
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    eprintln!("Browser microphone disconnected");
    Ok(())
}

/// The request's head, without consuming it (the WebSocket handshake needs to read it again).
fn peek_head(stream: &TcpStream) -> io::Result<String> {
    let mut buf = vec![0; 8192];
    for _ in 0..50 {
        let n = stream.peek(&mut buf)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&buf[..end + 4]).into_owned());
        }
        if n == buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(io::ErrorKind::TimedOut.into())
}

#[cfg(test)]
mod tests {
    use super::{Codec, Hub};
    use tungstenite::Bytes;

    #[test]
    fn slow_listeners_miss_frames_and_gone_ones_are_forgotten() {
        let hub = Hub::default();
        let slow = hub.join(Codec::Opus);
        let gone = hub.join(Codec::Opus);
        drop(gone);
        for _ in 0..super::QUEUE + 10 {
            hub.send(Codec::Opus, &Bytes::from_static(b"x"));
        }
        assert_eq!(slow.try_iter().count(), super::QUEUE);
        assert_eq!(hub.listeners.lock().unwrap().len(), 1);
        assert!(!hub.wants(Codec::Pcm));
    }

    #[test]
    fn mic_waits_for_40_ms_then_plays_and_fills_gaps_with_silence() {
        let mic = super::MicFeed::default();
        let mut out = vec![1i16; 960];
        mic.push(&[7; 960]);
        mic.take(&mut out);
        assert!(out.iter().all(|&s| s == 0)); // not enough yet
        mic.push(&[7; 960]);
        mic.take(&mut out);
        assert!(out.iter().all(|&s| s == 7));
        mic.take(&mut vec![0i16; 960]);
        mic.take(&mut out); // ran dry
        assert!(out.iter().all(|&s| s == 0));
        mic.push(&[1; 20_000]);
        assert_eq!(mic.queue.lock().unwrap().samples.len(), super::MIC_MAX);
    }
}
