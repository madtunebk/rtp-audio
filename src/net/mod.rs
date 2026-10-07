//! The network side, on every platform: RTP packets, their encryption, sending them, and finding
//! receivers.

pub mod discover;
pub mod rtp;
pub mod secure;
pub mod transport;
