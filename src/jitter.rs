//! Jitter buffer between the network and the sound card, plus the resampler that plays it at
//! the card's rate and slowly speeds up or slows down to stay near the target latency (the
//! sender's clock and the sound card's never run at exactly the same speed).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Packets this far ahead of the expected one are treated as lost and replaced by silence;
/// further means the sender restarted.
const MAX_GAP: u16 = 32;
/// Running out within this long of the last packet is a dropout; later, the sender just
/// paused (PulseAudio stops sending when nothing plays).
const PAUSE: Duration = Duration::from_millis(100);
/// Most the playback speed is changed to correct drift: 0.5% is not audible.
const MAX_ADJUST: f64 = 0.005;

pub struct Jitter {
    frames: VecDeque<[f32; 2]>,
    next_sequence: Option<u16>,
    playing: bool,
    /// Frames to collect before playing (and to aim for while playing).
    target: usize,
    /// Older frames are dropped beyond this: a stall must not leave the sound lagging behind.
    max: usize,
    last_push: Option<Instant>,
    pub stats: Stats,
}

/// Running totals of everything that can make the sound stutter.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets that never arrived (played as silence).
    pub lost: u64,
    /// Packets that arrived after their turn (dropped).
    pub late: u64,
    /// Times the buffer ran out while packets were still coming.
    pub underruns: u64,
    /// Times the buffer overfilled and old sound was skipped.
    pub trimmed: u64,
}

impl Jitter {
    pub fn new(target: usize) -> Self {
        Self {
            frames: VecDeque::with_capacity(target * 4),
            next_sequence: None,
            playing: false,
            target,
            max: target * 4,
            last_push: None,
            stats: Stats::default(),
        }
    }

    /// Add one packet of big-endian i16 samples with `channels` channels (1 or 2).
    pub fn push(&mut self, sequence: u16, payload: &[u8], channels: usize) {
        self.last_push = Some(Instant::now());
        let sample = |b: &[u8]| f32::from(i16::from_be_bytes([b[0], b[1]])) / 32768.0;
        let frames = payload.chunks_exact(2 * channels).map(|frame| {
            let left = sample(frame);
            [left, if channels == 2 { sample(&frame[2..]) } else { left }]
        });

        if let Some(expected) = self.next_sequence {
            match sequence.wrapping_sub(expected) {
                0 => {}
                gap if gap <= MAX_GAP => {
                    self.stats.lost += u64::from(gap);
                    let lost = gap as usize * payload.len() / (2 * channels);
                    self.frames.extend(std::iter::repeat_n([0.0; 2], lost));
                }
                gap if gap >= 0x8000 => {
                    self.stats.late += 1;
                    return;
                }
                _ => self.reset(),
            }
        }
        self.next_sequence = Some(sequence.wrapping_add(1));
        self.frames.extend(frames);
        if self.frames.len() > self.max {
            let excess = self.frames.len() - self.target;
            self.frames.drain(..excess);
            self.stats.trimmed += 1;
        }
    }

    fn reset(&mut self) {
        self.frames.clear();
        self.playing = false;
    }
}

/// Plays a Jitter buffer recorded at `input_rate` on a card running at `output_rate`.
pub struct Player {
    /// Input frames per output frame before drift correction.
    base_step: f64,
    adjust: f64,
    /// Position between frames[0] and frames[1].
    position: f64,
}

impl Player {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self { base_step: f64::from(input_rate) / f64::from(output_rate), adjust: 0.0, position: 0.0 }
    }

    /// Called once per sound card callback: nudge the speed towards the target fill level.
    pub fn update_speed(&mut self, jitter: &Jitter) {
        if !jitter.playing {
            return;
        }
        let error = (jitter.frames.len() as f64 - jitter.target as f64) / jitter.target as f64;
        let wanted = (error * 0.01).clamp(-MAX_ADJUST, MAX_ADJUST);
        self.adjust += (wanted - self.adjust) * 0.05;
    }

    /// The next stereo output frame, or silence while the buffer fills.
    pub fn next_frame(&mut self, jitter: &mut Jitter) -> [f32; 2] {
        if !jitter.playing {
            if jitter.frames.len() < jitter.target {
                return [0.0; 2];
            }
            jitter.playing = true;
            self.position = 0.0;
        }
        while self.position >= 1.0 && !jitter.frames.is_empty() {
            jitter.frames.pop_front();
            self.position -= 1.0;
        }
        let (Some(&a), Some(&b)) = (jitter.frames.front(), jitter.frames.get(1)) else {
            jitter.playing = false;
            if jitter.last_push.is_some_and(|at| at.elapsed() < PAUSE) {
                jitter.stats.underruns += 1;
            }
            return [0.0; 2];
        };
        let t = self.position as f32;
        self.position += self.base_step * (1.0 + self.adjust);
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    }
}

#[cfg(test)]
mod tests {
    use super::{Jitter, Player};

    fn packet(value: i16, frames: usize) -> Vec<u8> {
        std::iter::repeat_n(value.to_be_bytes(), frames * 2).flatten().collect()
    }

    #[test]
    fn waits_for_target_then_plays() {
        let mut jitter = Jitter::new(4);
        let mut player = Player::new(48_000, 48_000);
        jitter.push(0, &packet(16384, 2), 2);
        assert_eq!(player.next_frame(&mut jitter), [0.0; 2]);
        jitter.push(1, &packet(16384, 2), 2);
        assert_eq!(player.next_frame(&mut jitter), [0.5; 2]);
    }

    #[test]
    fn fills_lost_packets_with_silence_and_drops_late_ones() {
        let mut jitter = Jitter::new(100);
        jitter.push(10, &packet(1, 3), 2);
        jitter.push(12, &packet(1, 3), 2); // 11 lost: 3 silent frames
        assert_eq!(jitter.frames.len(), 9);
        jitter.push(11, &packet(1, 3), 2); // late
        assert_eq!(jitter.frames.len(), 9);
        assert_eq!((jitter.stats.lost, jitter.stats.late), (1, 1));
        jitter.push(5000, &packet(1, 3), 2); // sender restarted
        assert_eq!(jitter.frames.len(), 3);
    }

    #[test]
    fn mono_is_played_on_both_sides() {
        let mut jitter = Jitter::new(1);
        jitter.push(0, &packet(-16384, 2), 1);
        assert_eq!(Player::new(1, 1).next_frame(&mut jitter), [-0.5; 2]);
    }

    #[test]
    fn underrun_goes_silent_until_refilled() {
        let mut jitter = Jitter::new(2);
        let mut player = Player::new(48_000, 48_000);
        jitter.push(0, &packet(100, 2), 2);
        player.next_frame(&mut jitter);
        player.next_frame(&mut jitter);
        assert_eq!(player.next_frame(&mut jitter), [0.0; 2]);
        assert_eq!(jitter.stats.underruns, 1);
    }

    #[test]
    fn running_out_after_the_sender_stops_is_a_pause() {
        let mut jitter = Jitter::new(1);
        let mut player = Player::new(48_000, 48_000);
        jitter.push(0, &packet(100, 2), 2);
        jitter.last_push = Some(std::time::Instant::now() - super::PAUSE * 2);
        for _ in 0..3 {
            player.next_frame(&mut jitter);
        }
        assert_eq!(jitter.stats.underruns, 0);
    }
}
