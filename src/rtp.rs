//! RTP framing for 16-bit big-endian PCM (L16), as sent by PulseAudio's module-rtp-send,
//! PipeWire's module-rtp-sink or `ffmpeg -f rtp -acodec pcm_s16be`.

pub const HEADER_LEN: usize = 12;
/// Dynamic payload type, what PulseAudio and ffmpeg use for L16 at 48 kHz.
pub const PAYLOAD_TYPE: u8 = 97;

pub struct Packet<'a> {
    pub sequence: u16,
    /// Big-endian i16 samples, interleaved by channel.
    pub payload: &'a [u8],
}

/// Parse an RTP packet, skipping CSRCs, header extension and padding. None if it isn't RTP v2.
pub fn parse(data: &[u8]) -> Option<Packet<'_>> {
    if data.len() < HEADER_LEN || data[0] >> 6 != 2 {
        return None;
    }
    let csrc_count = (data[0] & 0x0f) as usize;
    let mut start = HEADER_LEN + csrc_count * 4;
    if data[0] & 0x10 != 0 {
        let words = u16::from_be_bytes([*data.get(start + 2)?, *data.get(start + 3)?]) as usize;
        start += 4 + words * 4;
    }
    let mut end = data.len();
    if data[0] & 0x20 != 0 {
        end = end.checked_sub(*data.last()? as usize)?;
    }
    let payload = data.get(start..end)?;
    Some(Packet { sequence: u16::from_be_bytes([data[2], data[3]]), payload })
}

/// Write an RTP v2 header for one packet.
pub fn header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; HEADER_LEN] {
    let mut out = [0; HEADER_LEN];
    out[0] = 2 << 6;
    out[1] = PAYLOAD_TYPE;
    out[2..4].copy_from_slice(&sequence.to_be_bytes());
    out[4..8].copy_from_slice(&timestamp.to_be_bytes());
    out[8..12].copy_from_slice(&ssrc.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::{header, parse};

    #[test]
    fn round_trips_a_packet() {
        let mut packet = header(513, 960, 7).to_vec();
        packet.extend_from_slice(&[1, 2, 3, 4]);
        let parsed = parse(&packet).unwrap();
        assert_eq!(parsed.sequence, 513);
        assert_eq!(parsed.payload, [1, 2, 3, 4]);
    }

    #[test]
    fn skips_csrcs_extension_and_padding() {
        // v2, padding, extension, 1 CSRC; extension of 1 word; 2 bytes of padding.
        let mut packet = vec![0b1011_0001, 97, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0];
        packet.extend_from_slice(&[9, 9, 9, 9]); // CSRC
        packet.extend_from_slice(&[0, 0, 0, 1, 8, 8, 8, 8]); // extension
        packet.extend_from_slice(&[1, 2, 0, 2]); // payload + padding
        assert_eq!(parse(&packet).unwrap().payload, [1, 2]);
    }

    #[test]
    fn rejects_non_rtp() {
        assert!(parse(&[0; 4]).is_none());
        assert!(parse(&[0; 20]).is_none());
    }
}
