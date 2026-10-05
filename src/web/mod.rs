//! Sound for web browsers: the captured audio as Opus over a WebSocket, plus the small player
//! script that plays it, on one local port. Meant to sit behind NGINX, which adds HTTPS and the
//! login (see docs/web.md):
//!
//!   GET /player.js   the player: adds a sound button to the page that loads it
//!   GET /            a bare page with just the player, for testing
//!   GET /ws          the WebSocket: one text message describing the stream, then one binary
//!                    message per 20 ms frame (Opus, or s16le PCM with ?codec=pcm)

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tungstenite::{Bytes, Message};

use crate::transport::AudioSink;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms frames: Opus's usual size, and what the player expects.
const FRAME_SAMPLES: usize = RATE as usize / 50 * CHANNELS;
/// Frames queued per listener (0.5 s); beyond that a slow listener misses frames instead of
/// slowing down everyone else.
const QUEUE: usize = 25;
const BITRATE: i32 = 128_000;
const PLAYER: &str = include_str!("player.js");
const PAGE: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
    <title>rtp-audio</title><body style=\"background:#111;color:#ddd;font:16px sans-serif;padding:2em\">\
    <p>rtp-audio: press the sound button in the corner.</p><script src=\"player.js\"></script>";

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

/// Encodes the captured sound once and hands it to every browser listening.
pub struct WebSink {
    hub: Arc<Hub>,
    encoder: opus::Encoder,
    frame: Vec<i16>,
    packet: Vec<u8>,
}

/// Start serving browsers on `address`. Returns the sink to feed captured sound into.
pub fn start(address: SocketAddr) -> Result<WebSink, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(address).map_err(|err| format!("cannot listen on {address}: {err}"))?;
    let hub = Arc::new(Hub::default());
    let server_hub = Arc::clone(&hub);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let hub = Arc::clone(&server_hub);
            std::thread::spawn(move || {
                if let Err(err) = serve(stream, &hub) {
                    eprintln!("Web listener: {err}");
                }
            });
        }
    });
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)?;
    encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
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
fn serve(mut stream: TcpStream, hub: &Hub) -> Result<(), Box<dyn std::error::Error>> {
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
        let mut socket = tungstenite::accept(stream)?;
        socket.get_mut().set_read_timeout(None)?;
        let name = if codec == Codec::Opus { "opus" } else { "pcm" };
        socket.send(Message::text(format!(
            r#"{{"codec":"{name}","sampleRate":{RATE},"channels":{CHANNELS},"frameMs":20}}"#
        )))?;
        let frames = hub.join(codec);
        // Ends when the browser goes away (the send fails) or the sender stops.
        while let Ok(frame) = frames.recv() {
            if socket.send(Message::Binary(frame)).is_err() {
                break;
            }
        }
        return Ok(());
    }

    // Consume the request we only peeked at, then answer it.
    let mut discard = vec![0; head.len()];
    stream.read_exact(&mut discard)?;
    let (status, kind, body) = match path.split('?').next().unwrap_or("/") {
        "/player.js" => ("200 OK", "text/javascript; charset=utf-8", PLAYER),
        "/" => ("200 OK", "text/html; charset=utf-8", PAGE),
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
}
