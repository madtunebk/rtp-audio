//! Encrypted RTP (`--key`): every packet is sealed with ChaCha20-Poly1305 and a pre-shared key.
//!
//! An encrypted packet is an RTP header with payload type 120, an 8-byte packet counter, then
//! the sealed body: the real payload type (1 byte) followed by the payload, plus a 16-byte tag.
//! The nonce is SSRC (4 bytes) + counter (8 bytes). The counter only grows within a session, and
//! each session starts with a random SSRC and a random 62-bit counter, so sessions sharing a key
//! practically never reuse a nonce. The header and counter are authenticated too, so nothing in
//! the packet can be changed. This is rtp-audio's own format, not SRTP: both ends must be
//! rtp-audio.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

use crate::rtp;

pub const PAYLOAD_TYPE: u8 = 120;
const COUNTER_LEN: usize = 8;
const KEY_LEN: usize = 32;

#[derive(Clone)]
pub struct Key(ChaCha20Poly1305);

impl Key {
    /// A key as printed by `rtp-audio keygen`: 32 bytes in base64.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bytes = base64_decode(text.trim()).ok_or("the key is not valid base64 (make one with `rtp-audio keygen`)")?;
        if bytes.len() != KEY_LEN {
            return Err(format!("the key must be {KEY_LEN} bytes; this one is {} (make one with `rtp-audio keygen`)", bytes.len()));
        }
        Ok(Self(ChaCha20Poly1305::new_from_slice(&bytes).expect("32-byte key")))
    }

    /// Read a key from a file (its first line).
    pub fn from_file(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|err| format!("cannot read the key file {path}: {err}"))?;
        Self::parse(text.lines().next().unwrap_or_default())
    }

    /// Wrap a plain RTP packet's payload into an encrypted packet.
    pub fn seal(&self, header: &[u8; rtp::HEADER_LEN], counter: u64, payload_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut header = *header;
        header[1] = (header[1] & 0x80) | PAYLOAD_TYPE;
        let mut aad = Vec::with_capacity(rtp::HEADER_LEN + COUNTER_LEN);
        aad.extend_from_slice(&header);
        aad.extend_from_slice(&counter.to_be_bytes());
        let mut plain = Vec::with_capacity(1 + payload.len());
        plain.push(payload_type);
        plain.extend_from_slice(payload);
        let sealed = self.0.encrypt(&nonce(&header, counter), Payload { msg: &plain, aad: &aad }).expect("encryption cannot fail");
        let mut packet = aad;
        packet.extend_from_slice(&sealed);
        packet
    }

    /// Open an encrypted packet: the counter, the real payload type and the payload. None if it
    /// is not ours, was changed, or used another key.
    pub fn open(&self, packet: &[u8]) -> Option<(u64, u8, Vec<u8>)> {
        let header: [u8; rtp::HEADER_LEN] = packet.get(..rtp::HEADER_LEN)?.try_into().ok()?;
        if header[1] & 0x7f != PAYLOAD_TYPE {
            return None;
        }
        let aad = packet.get(..rtp::HEADER_LEN + COUNTER_LEN)?;
        let counter = u64::from_be_bytes(aad[rtp::HEADER_LEN..].try_into().ok()?);
        let sealed = &packet[aad.len()..];
        let plain = self.0.decrypt(&nonce(&header, counter), Payload { msg: sealed, aad }).ok()?;
        let (&payload_type, payload) = plain.split_first()?;
        Some((counter, payload_type, payload.to_vec()))
    }
}

fn nonce(header: &[u8; rtp::HEADER_LEN], counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&header[8..12]);
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(nonce)
}

/// `rtp-audio keygen`: a new random key.
pub fn generate() -> Result<String, String> {
    let mut bytes = [0u8; KEY_LEN];
    getrandom::fill(&mut bytes).map_err(|err| format!("cannot get random bytes: {err}"))?;
    Ok(base64_encode(&bytes))
}

/// How many senders the replay window remembers at once.
const SENDERS: usize = 32;

/// Rejects packets seen before (replays) or too old to tell, for each of the last senders: a
/// recording of an earlier session, mixed in with the current one, can't be played again.
/// (A receiver started after a session can't tell its recording from a live sender: there is no
/// handshake. Use a new key to make old recordings useless.)
#[derive(Default)]
pub struct ReplayWindow {
    sessions: Vec<Session>,
    clock: u64,
}

struct Session {
    ssrc: u32,
    highest: u64,
    /// Bit i: counter `highest - i` was seen.
    seen: u64,
    /// When it was last heard from, to forget the oldest.
    used: u64,
}

impl ReplayWindow {
    pub fn accept(&mut self, ssrc: u32, counter: u64) -> bool {
        self.clock += 1;
        let Some(session) = self.sessions.iter_mut().find(|s| s.ssrc == ssrc) else {
            if self.sessions.len() == SENDERS
                && let Some(oldest) = (0..self.sessions.len()).min_by_key(|&i| self.sessions[i].used)
            {
                self.sessions.swap_remove(oldest);
            }
            self.sessions.push(Session { ssrc, highest: counter, seen: 1, used: self.clock });
            return true;
        };
        session.used = self.clock;
        if counter > session.highest {
            let shift = counter - session.highest;
            session.seen = if shift >= 64 { 1 } else { (session.seen << shift) | 1 };
            session.highest = counter;
            return true;
        }
        let age = session.highest - counter;
        if age >= 64 || session.seen & (1 << age) != 0 {
            return false;
        }
        session.seen |= 1 << age;
        true
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard or URL-safe base64, padding optional.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|&c| c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::parse(&generate().unwrap()).unwrap()
    }

    #[test]
    fn base64_round_trips() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_decode("TWE").unwrap(), b"Ma");
        assert!(base64_decode("not base64!").is_none());
        assert!(Key::parse("TWFu").is_err());
    }

    #[test]
    fn seals_and_opens_and_rejects_changes() {
        let key = key();
        let header = rtp::header(7, 960, 0xdead_beef, 111);
        let packet = key.seal(&header, 42, 111, b"opus frame");
        assert_eq!(packet[1] & 0x7f, PAYLOAD_TYPE);
        assert_eq!(key.open(&packet), Some((42, 111, b"opus frame".to_vec())));
        // Any changed byte, the wrong key, or a cut packet: rejected.
        for i in 0..packet.len() {
            let mut bad = packet.clone();
            bad[i] ^= 1;
            assert_eq!(key.open(&bad), None, "byte {i}");
        }
        assert_eq!(super::tests::key().open(&packet), None);
        assert_eq!(key.open(&packet[..20]), None);
    }

    #[test]
    fn replay_window() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(1, 10));
        assert!(!window.accept(1, 10)); // replayed
        assert!(window.accept(1, 12));
        assert!(window.accept(1, 11)); // late but new
        assert!(!window.accept(1, 11));
        assert!(window.accept(1, 200));
        assert!(!window.accept(1, 100)); // too old to tell
        assert!(window.accept(2, 5)); // another sender starts over
    }

    #[test]
    fn replays_from_alternating_sessions_are_rejected() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(0xA, 100));
        assert!(window.accept(0xB, 7));
        // Recordings of both sessions, played again in turn.
        assert!(!window.accept(0xA, 100));
        assert!(!window.accept(0xB, 7));
        assert!(window.accept(0xA, 101));
    }

    #[test]
    fn the_oldest_sender_is_forgotten_first() {
        let mut window = ReplayWindow::default();
        for ssrc in 0..SENDERS as u32 {
            assert!(window.accept(ssrc, 1));
        }
        assert!(window.accept(0, 2)); // sender 0 is now the most recent
        assert!(window.accept(1000, 1)); // makes room by forgetting sender 1
        assert!(!window.accept(0, 2));
        assert!(window.accept(1, 1)); // forgotten, so it looks new
    }
}
