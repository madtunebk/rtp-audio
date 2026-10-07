//! Finding receivers on the local network: `rtp-audio find` broadcasts a question to the
//! receivers' port, and each receiver answers with its name. Broadcasts stay on the local
//! network, so over a VPN give the address instead.
//!
//! Question: `RTP-AUDIO? 1`. Answer: `RTP-AUDIO! 1 port=46000 key=0 name=<computer name>`.
//! Neither starts like RTP (version 2), so a receiver tells them from sound at once.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const QUESTION: &[u8] = b"RTP-AUDIO? 1";
const ANSWER: &str = "RTP-AUDIO! 1";

/// A receiver that answered.
#[derive(Debug, PartialEq)]
pub struct Found {
    pub name: String,
    pub address: SocketAddr,
    pub needs_key: bool,
}

/// Is this datagram a discovery question?
pub fn is_question(data: &[u8]) -> bool {
    data == QUESTION
}

/// The answer a receiver on `port` gives.
pub fn answer(port: u16, needs_key: bool) -> Vec<u8> {
    let name = gethostname::gethostname().to_string_lossy().replace(char::is_whitespace, "-");
    format!("{ANSWER} port={port} key={} name={name}", u8::from(needs_key)).into_bytes()
}

fn parse_answer(data: &[u8], from: SocketAddr) -> Option<Found> {
    let text = std::str::from_utf8(data).ok()?.strip_prefix(ANSWER)?;
    let (mut port, mut needs_key, mut name) = (None, false, String::new());
    for field in text.split_whitespace() {
        match field.split_once('=')? {
            ("port", value) => port = value.parse().ok(),
            ("key", value) => needs_key = value == "1",
            ("name", value) => name = value.to_string(),
            _ => {}
        }
    }
    Some(Found { name, address: SocketAddr::new(from.ip(), port?), needs_key })
}

/// Ask the local network for receivers on `port`, listening for answers for `wait`.
pub fn find(port: u16, wait: Duration) -> std::io::Result<Vec<Found>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    // Some systems (macOS) refuse the general broadcast address, so also ask this network's own
    // broadcast address (assuming the usual /24), and this computer itself, which a broadcast
    // doesn't always reach. Only fail if nothing could be asked.
    let mut asked = Vec::new();
    let mut targets = vec![Ipv4Addr::BROADCAST, Ipv4Addr::LOCALHOST];
    if let Some(ip) = local_ipv4() {
        let [a, b, c, _] = ip.octets();
        targets.insert(1, Ipv4Addr::new(a, b, c, 255));
    }
    for target in targets {
        asked.push(socket.send_to(QUESTION, (target, port)));
    }
    if let Some(Err(err)) = asked.iter().find(|r| r.is_err()).filter(|_| asked.iter().all(Result::is_err)) {
        return Err(std::io::Error::new(err.kind(), err.to_string()));
    }
    let deadline = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 512];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        socket.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let Some(receiver) = parse_answer(&buf[..len], from) else { continue };
                // Each address once. A receiver on this computer may also answer on localhost: keep
                // its network address. Two computers with the same name (clones) stay two.
                let same_here = |f: &Found| {
                    f.name == receiver.name
                        && f.address.port() == receiver.address.port()
                        && (f.address.ip().is_loopback() || receiver.address.ip().is_loopback())
                };
                match found.iter().position(|f| f.address == receiver.address || same_here(f)) {
                    Some(i) if found[i].address.ip().is_loopback() => found[i] = receiver,
                    Some(_) => {}
                    None => found.push(receiver),
                }
            }
            Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(err) => return Err(err),
        }
    }
    Ok(found)
}

/// This computer's address on its main network: the one a packet to the internet would come
/// from (nothing is sent).
fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// `rtp-audio find`.
pub fn print_found(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let found = find(port, Duration::from_secs(1))?;
    if found.is_empty() {
        println!("No receiver answered on UDP port {port}.");
        println!("Is `rtp-audio` running on the other computer, on the same local network? (Over a VPN, use its address.)");
        return Ok(());
    }
    let width = found.iter().map(|f| f.name.len()).max().unwrap_or(0);
    for f in &found {
        println!("{:width$}  {}{}", f.name, f.address, if f.needs_key { "  (needs a key)" } else { "" });
    }
    Ok(())
}

/// The receiver `rtp-audio send auto` should use: the only one that answers.
pub fn pick(port: u16) -> Result<SocketAddr, String> {
    let found = find(port, Duration::from_secs(1)).map_err(|err| format!("cannot look for receivers: {err}"))?;
    match found.as_slice() {
        [one] => {
            eprintln!("Found receiver {} at {}", one.name, one.address);
            Ok(one.address)
        }
        [] => Err(format!(
            "no receiver answered on UDP port {port}: is `rtp-audio` running on the other computer, on the same \
             local network? Over a VPN, give its address instead of `auto`."
        )),
        several => Err(format!(
            "several receivers answered; pick one:\n{}",
            several.iter().map(|f| format!("  rtp-audio send {}   ({})", f.address, f.name)).collect::<Vec<_>>().join("\n")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_and_are_not_rtp() {
        let from: SocketAddr = "192.168.1.20:46000".parse().unwrap();
        let found = parse_answer(&answer(46001, true), from).unwrap();
        assert_eq!((found.address.to_string(), found.needs_key), ("192.168.1.20:46001".to_string(), true));
        assert!(!found.name.is_empty());
        assert!(parse_answer(b"something else", from).is_none());
        assert!(is_question(QUESTION) && !is_question(b"RTP-AUDIO? 2"));
        // A receiver must never mistake either for sound: not RTP version 2.
        assert!(crate::net::rtp::parse(QUESTION).is_none());
        assert!(crate::net::rtp::parse(&answer(1, false)).is_none());
    }

    #[test]
    fn finds_a_receiver_answering_on_this_computer() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = receiver.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok((len, from)) = receiver.recv_from(&mut buf) {
                if is_question(&buf[..len]) {
                    receiver.send_to(&answer(port, false), from).unwrap();
                }
            }
        });
        let found = find(port, Duration::from_millis(300)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].address.port(), port);
    }
}
