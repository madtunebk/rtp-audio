//! The receiver over TCP: the sender's web stream (`send --web`), at a ws:// URL.

use std::error::Error;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use tungstenite::Message;

use super::status::{Monitor, STATUS_EVERY, STATUS_WIDTH};
use super::{Buffers, Options, print_volume_and_quit};

/// How long to wait before connecting again after the sender's stream ends or can't be reached.
pub(super) const RECONNECT_EVERY: Duration = Duration::from_secs(2);

/// The sender's WebSocket stream (`rtp-audio send --web`): Opus over TCP, so nothing is lost
/// and it goes through an SSH tunnel or a proxy; it reconnects by itself when the sender restarts.
pub(super) fn receive_websocket(url: &str, options: &Options, card: &str, jitter: &Buffers, peak: &AtomicU32, monitor: &mut Monitor) -> Result<(), Box<dyn Error>> {
    let target = WsTarget::parse(url)?;
    println!("Receiving from {url} over TCP ({} ms buffer); {card}", options.latency_ms);
    print_volume_and_quit(options);
    let mut last_problem = String::new();
    // Frame numbers for the jitter buffer, carried across reconnections: starting again from 0
    // would look like old, late sound to it.
    let mut sequence: u16 = 0;
    loop {
        let problem = match websocket_session(&target, options, jitter, peak, monitor, &mut sequence) {
            Ok(()) => "the sender closed the connection".to_string(),
            Err(err) => err.to_string(),
        };
        if problem != last_problem {
            if monitor.live {
                print!("\r{:width$}\r", "", width = STATUS_WIDTH);
            }
            println!("{url}: {problem}; trying again every {} s", RECONNECT_EVERY.as_secs());
            last_problem = problem;
        }
        monitor.forget_sender();
        let retry = Instant::now() + RECONNECT_EVERY;
        while Instant::now() < retry {
            std::thread::sleep(STATUS_EVERY);
            monitor.tick(jitter, peak);
        }
    }
}

/// One connection to the sender's stream, until it ends.
pub(super) fn websocket_session(target: &WsTarget, options: &Options, jitter: &Buffers, peak: &AtomicU32, monitor: &mut Monitor, sequence: &mut u16) -> Result<(), Box<dyn Error>> {
    let stream = TcpStream::connect((target.host.as_str(), target.port))?;
    // Frames are small and every 20 ms: send them at once rather than gathering them.
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let peer = stream.peer_addr()?.to_string();
    let (mut socket, _) = tungstenite::client(target.request.as_str(), stream)
        .map_err(|err| format!("no rtp-audio stream there ({err})"))?;

    // First a text message saying what follows: {"codec":"opus","sampleRate":48000,"channels":2,…}.
    let header = loop {
        match socket.read()? {
            Message::Text(text) => break text.to_string(),
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    };
    if !header.contains(r#""codec":"opus""#) {
        return Err(format!("unexpected stream from the sender: {header}").into());
    }
    let channels = header
        .split(r#""channels":"#)
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(2);
    if channels != options.channels {
        return Err(format!("the sender sends {channels} channel(s); start the receiver with --channels {channels}").into());
    }
    let mut decoder = opus::Decoder::new(48_000, if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo })?;
    socket.get_mut().set_read_timeout(Some(STATUS_EVERY))?;

    let mut pcm = vec![0i16; 5760 * channels];
    loop {
        match socket.read() {
            Ok(Message::Binary(data)) => {
                let samples = decoder.decode(&data, &mut pcm, false)?;
                let frame: Vec<u8> = pcm[..samples * channels].iter().flat_map(|s| s.to_be_bytes()).collect();
                jitter.push(*sequence, &frame, channels);
                *sequence = sequence.wrapping_add(1);
                monitor.arrived(&peer, || "Opus over TCP".to_string());
            }
            // A sender stopped with Ctrl+C just drops the connection: that's an ordinary end too.
            Ok(Message::Close(_))
            | Err(tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake)) => return Ok(()),
            Err(tungstenite::Error::Io(err)) if err.kind() == ErrorKind::ConnectionReset => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(err)) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => return Err(err.into()),
        }
        monitor.tick(jitter, peak);
    }
}

/// Where a `ws://HOST[:PORT][/PATH]` URL points. The sender serves its stream at /ws; a URL that
/// ends in / (e.g. behind a proxy at /audio/) gets ws added.
pub(super) struct WsTarget {
    host: String,
    port: u16,
    request: String,
}

impl WsTarget {
    fn parse(url: &str) -> Result<Self, String> {
        if url.starts_with("wss://") {
            return Err("wss:// isn't supported yet: use ws:// through an SSH tunnel (ssh -L 46080:localhost:46080 SERVER)".into());
        }
        let rest = url.strip_prefix("ws://").ok_or_else(|| format!("'{url}' is not a ws:// URL"))?;
        let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        // An IPv6 address comes in brackets ([::1]:46080), since it has colons of its own.
        let (host, port) = if let Some(inside) = authority.strip_prefix('[') {
            let (host, after) = inside.split_once(']').ok_or_else(|| format!("unclosed [ in '{url}'"))?;
            let port = match after.strip_prefix(':') {
                Some(port) => port.parse::<u16>().map_err(|_| format!("bad port in '{url}'"))?,
                None if after.is_empty() => 80,
                None => return Err(format!("bad address in '{url}'")),
            };
            (host, port)
        } else {
            match authority.split_once(':') {
                Some((_, port)) if port.contains(':') => {
                    return Err(format!("put an IPv6 address in brackets: ws://[{authority}]/ or ws://[ADDRESS]:PORT"));
                }
                Some((host, port)) => (host, port.parse::<u16>().map_err(|_| format!("bad port in '{url}'"))?),
                None => (authority, 80),
            }
        };
        if host.is_empty() {
            return Err(format!("no host in '{url}'"));
        }
        let path = if path.ends_with('/') { format!("{path}ws") } else { path.to_string() };
        Ok(WsTarget { host: host.to_string(), port, request: format!("ws://{authority}{path}") })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn websocket_urls() {
        let t = super::WsTarget::parse("ws://localhost:46080").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.request.as_str()), ("localhost", 46080, "ws://localhost:46080/ws"));
        let t = super::WsTarget::parse("ws://10.0.0.2/audio/").unwrap();
        assert_eq!((t.port, t.request.as_str()), (80, "ws://10.0.0.2/audio/ws"));
        assert_eq!(super::WsTarget::parse("ws://h:1/custom").unwrap().request, "ws://h:1/custom");
        assert!(super::WsTarget::parse("wss://h/").is_err());
        assert!(super::WsTarget::parse("ws://:5").is_err());
        assert!(super::WsTarget::parse("http://h").is_err());
        let t = super::WsTarget::parse("ws://[::1]:46080").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.request.as_str()), ("::1", 46080, "ws://[::1]:46080/ws"));
        assert_eq!(super::WsTarget::parse("ws://[fe80::1]/audio/").unwrap().port, 80);
        assert!(super::WsTarget::parse("ws://::1:46080").is_err()); // needs brackets
        assert!(super::WsTarget::parse("ws://[::1").is_err());
    }
}
