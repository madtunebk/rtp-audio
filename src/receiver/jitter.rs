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
    #[cfg(test)]
    next_sequence: Option<u16>,
    playing: bool,
    /// Frames to collect before playing (and to aim for while playing).
    target: usize,
    /// Older frames are dropped beyond this: a stall must not leave the sound lagging behind.
    max: usize,
    /// Frames room was made for up front (the callback never allocates): the most max can be.
    capacity: usize,
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
    /// Times the sound card itself reported a problem (an underrun or overrun).
    pub card: u64,
}

impl Jitter {
    /// Frames waiting to be played.
    pub fn buffered(&self) -> usize {
        self.frames.len()
    }

    #[cfg(test)]
    pub fn new(target: usize) -> Self {
        Self::with_room(target, 0)
    }

    /// A buffer whose target may later grow by up to `room` frames (--sync) without dropping sound.
    pub fn with_room(target: usize, room: usize) -> Self {
        let capacity = (target.max(2) + room) * 4;
        Self {
            frames: VecDeque::with_capacity(capacity),
            #[cfg(test)]
            next_sequence: None,
            playing: false,
            target: target.max(2),
            max: target.max(2) * 4,
            capacity,
            last_push: None,
            stats: Stats::default(),
        }
    }

    /// Add one packet of big-endian i16 samples with `channels` channels (1 or 2).
    #[cfg(test)]
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
                    for _ in 0..lost {
                        self.append([0.0; 2]);
                    }
                }
                gap if gap >= 0x8000 => {
                    self.stats.late += 1;
                    return;
                }
                _ => self.reset(),
            }
        }
        self.next_sequence = Some(sequence.wrapping_add(1));
        for frame in frames {
            self.append(frame);
        }
        if self.frames.len() > self.max {
            let excess = self.frames.len() - self.target;
            self.frames.drain(..excess);
            self.stats.trimmed += 1;
        }
    }

    // The audio callback must never grow its allocation, even on a network burst.
    fn append(&mut self, frame: [f32; 2]) {
        if self.frames.len() == self.max {
            let excess = self.frames.len().saturating_sub(self.target);
            self.frames.drain(..excess.max(1));
            self.stats.trimmed += 1;
        }
        self.frames.push_back(frame);
    }

    /// Start over for a new sender: drop the buffered sound and forget the sequence numbers.
    pub fn restart(&mut self) {
        self.reset();
        #[cfg(test)]
        {
            self.next_sequence = None;
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
            // Start exactly at the target: sound gathered while the output was still opening would
            // otherwise take its time to play off (at 0.5% faster), leaving this output late,
            // and behind the others when there are several.
            // (Two at least: playing interpolates between neighbouring frames.)
            let excess = jitter.frames.len() - jitter.target.max(2).min(jitter.frames.len());
            jitter.frames.drain(..excess);
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

/// Shared counters only: neither network nor telemetry can lock the output callback.
#[derive(Default)]
pub(super) struct Metrics {
    pub(super) card: std::sync::atomic::AtomicU64,
    generation: std::sync::atomic::AtomicU64,
    lost: std::sync::atomic::AtomicU64,
    late: std::sync::atomic::AtomicU64,
    overruns: std::sync::atomic::AtomicU64,
    underruns: std::sync::atomic::AtomicU64,
    trimmed: std::sync::atomic::AtomicU64,
    buffered: std::sync::atomic::AtomicUsize,
    /// How long the sound card (or sound server) takes to play what the callback writes, in µs:
    /// cpal's playback minus callback instant.
    pub(super) device_latency_us: std::sync::atomic::AtomicU64,
    /// With --sync: how much more sound this output keeps buffered (frames) to play together with
    /// the slowest output.
    pub(super) sync_frames: std::sync::atomic::AtomicUsize,
}
impl Metrics {
    pub(super) fn state(&self) -> (Stats, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            Stats {
                card: self.card.load(Relaxed),
                lost: self.lost.load(Relaxed),
                late: self.late.load(Relaxed),
                underruns: self.underruns.load(Relaxed),
                trimmed: self.trimmed.load(Relaxed) + self.overruns.load(Relaxed),
            },
            self.buffered.load(Relaxed),
        )
    }

    /// The latency of the output's device, in ms.
    pub(super) fn device_latency_ms(&self) -> f32 {
        self.device_latency_us.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0
    }
}
#[derive(Clone, Copy)]
struct Frame {
    samples: [f32; 2],
    generation: u64,
}

/// Network-owned producer. Full queues drop the new packet (counted), never block.
pub(super) struct Feed {
    producer: rtrb::Producer<Frame>,
    pub(super) metrics: std::sync::Arc<Metrics>,
    pub(super) errors: rtrb::Consumer<cpal::Error>,
    sequence: Option<u16>,
    generation: u64,
}
/// Callback-owned jitter and ring consumer. No mutexes, logging or allocations.
pub(super) struct Playback {
    consumer: rtrb::Consumer<Frame>,
    jitter: Jitter,
    /// The buffer this output aims for, before --sync adds to it.
    base: usize,
    generation: u64,
    pub(super) metrics: std::sync::Arc<Metrics>,
}
/// An output's queue and buffer, aiming for `target` frames, with `room` frames more that --sync may
/// add.
pub(super) fn channel(target: usize, room: usize) -> (Feed, Playback, rtrb::Producer<cpal::Error>) {
    let target = target.max(2);
    let (producer, consumer) = rtrb::RingBuffer::new((target + room) * 4);
    let (errors, read_errors) = rtrb::RingBuffer::new(16);
    let metrics = std::sync::Arc::new(Metrics::default());
    (
        Feed { producer, metrics: metrics.clone(), errors: read_errors, sequence: None, generation: 0 },
        Playback { consumer, jitter: Jitter::with_room(target, room), base: target, generation: 0, metrics },
        errors,
    )
}
impl Feed {
    pub(super) fn push(&mut self, sequence: u16, pcm: &[u8], channels: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        let count = pcm.len() / (2 * channels);
        let mut gap = 0;
        if let Some(expected) = self.sequence {
            match sequence.wrapping_sub(expected) {
                0 => {}
                n if n <= MAX_GAP => {
                    gap = usize::from(n);
                    self.metrics.lost.fetch_add(u64::from(n), Relaxed);
                }
                n if n >= 0x8000 => {
                    self.metrics.late.fetch_add(1, Relaxed);
                    return;
                }
                _ => self.restart(),
            }
        }
        self.sequence = Some(sequence.wrapping_add(1));
        // Do not partially enqueue a packet: that would warp the clock/sample count.
        if self.producer.slots() < count.saturating_mul(gap + 1) {
            self.metrics.overruns.fetch_add(1, Relaxed);
            return;
        }
        for _ in 0..count * gap {
            let _ = self.producer.push(Frame { samples: [0.; 2], generation: self.generation });
        }
        for bytes in pcm.chunks_exact(2 * channels) {
            let left = f32::from(i16::from_be_bytes([bytes[0], bytes[1]])) / 32768.;
            let right = if channels == 2 { f32::from(i16::from_be_bytes([bytes[2], bytes[3]])) / 32768. } else { left };
            let _ = self.producer.push(Frame { samples: [left, right], generation: self.generation });
        }
    }
    pub(super) fn restart(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.sequence = None;
        self.metrics.generation.store(self.generation, std::sync::atomic::Ordering::Release);
    }
    pub(super) fn count_lost(&self, n: u16) {
        self.metrics.lost.fetch_add(u64::from(n), std::sync::atomic::Ordering::Relaxed);
    }
}
impl Playback {
    pub(super) fn refill(&mut self) {
        // Snapshot once: concurrent arrivals cannot make this callback drain forever.
        let generation = self.metrics.generation.load(std::sync::atomic::Ordering::Acquire);
        if generation != self.generation {
            self.jitter.restart();
            self.generation = generation;
        }
        let count = self.consumer.slots().min(8192);
        let mut received = false;
        for _ in 0..count {
            if let Ok(frame) = self.consumer.pop() {
                if frame.generation != self.generation {
                    continue;
                }
                self.jitter.append(frame.samples);
                received = true;
            }
        }
        if received {
            self.jitter.last_push = Some(Instant::now());
        }
    }

    pub(super) fn prepare_callback(&mut self, player: &Player, frames: usize) {
        // A short --latency must still cover one hardware callback plus interpolation.
        // Keep the preallocated bound even if a backend unexpectedly asks for a huge block.
        let needed = (frames as f64 * player.base_step * (1.0 + MAX_ADJUST)).ceil() as usize + 2;
        // --sync's share comes in through the speed adjustment, or at once before playing.
        let wanted = self.base + self.metrics.sync_frames.load(std::sync::atomic::Ordering::Relaxed);
        let target = wanted.max(needed).min(self.jitter.capacity - 1);
        self.jitter.target = target;
        // Bursts are trimmed beyond 4× the target, as long as the room made up front allows: a
        // target raised by --sync must not sit at that limit, or every packet would be trimmed.
        self.jitter.max = (target * 4).clamp(target + 1, self.jitter.capacity);
    }
    pub(super) fn update_speed(&self, player: &mut Player) {
        player.update_speed(&self.jitter);
    }
    pub(super) fn next_frame(&mut self, player: &mut Player) -> [f32; 2] {
        player.next_frame(&mut self.jitter)
    }
    pub(super) fn publish(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.metrics.underruns.store(self.jitter.stats.underruns, Relaxed);
        self.metrics.trimmed.store(self.jitter.stats.trimmed, Relaxed);
        self.metrics.buffered.store(self.jitter.buffered() + self.consumer.slots(), Relaxed);
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
    fn starts_at_the_target_whatever_gathered_before() {
        let mut jitter = Jitter::new(4);
        jitter.push(0, &packet(100, 10), 2); // 10 frames gathered while the output was opening
        Player::new(48_000, 48_000).next_frame(&mut jitter);
        assert!(jitter.buffered() <= 4);
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
    #[test]
    fn spsc_plays_and_counts_missing_and_late_packets() {
        let (mut feed, mut output, _errors) = super::channel(100, 0);
        feed.push(10, &packet(16384, 40), 2);
        feed.push(12, &packet(16384, 40), 2);
        feed.push(11, &packet(16384, 40), 2);
        output.refill();
        let mut player = Player::new(48000, 48000);
        // Startup trims 120 frames to the target; the first 20 are from packet 10.
        assert_eq!(output.next_frame(&mut player), [0.5; 2]);
        output.publish();
        let (stats, _) = feed.metrics.state();
        assert_eq!((stats.lost, stats.late), (1, 1));
    }

    #[test]
    fn full_spsc_queue_never_waits_and_does_not_partially_insert() {
        let (mut feed, mut output, _errors) = super::channel(4, 0);
        feed.push(0, &packet(100, 16), 2);
        feed.push(1, &packet(200, 2), 2);
        assert_eq!(feed.producer.slots(), 0);
        assert_eq!(feed.metrics.state().0.trimmed, 1);
        let capacity = output.jitter.frames.capacity();
        output.refill();
        assert_eq!(output.jitter.frames.capacity(), capacity);
        assert_eq!(output.jitter.frames.len(), 16);
        // Larger later batches also cannot reallocate the callback-owned queue.
        feed.push(2, &packet(300, 16), 2);
        output.refill();
        assert_eq!(output.jitter.frames.capacity(), capacity);
        assert!(output.jitter.frames.len() <= 16);
    }

    #[test]
    fn stalled_producer_does_not_hold_the_callback() {
        let (feed, mut output, _errors) = super::channel(2880, 0);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            let _feed = feed;
            ready_tx.send(()).unwrap();
            done_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        output.refill();
        let mut player = Player::new(48000, 48000);
        for _ in 0..1920 {
            assert_eq!(output.next_frame(&mut player), [0.; 2]);
        }
        output.publish();
        // No timing threshold: completion happens while producer is still blocked.
        done_tx.send(()).unwrap();
        producer.join().unwrap();
    }

    #[test]
    fn restart_discards_pending_old_sender_without_waiting_for_new_audio() {
        let (mut feed, mut output, _errors) = super::channel(4, 0);
        feed.push(0, &packet(16384, 4), 2);
        output.refill();
        let mut player = Player::new(48000, 48000);
        assert_eq!(output.next_frame(&mut player), [0.5; 2]);
        feed.push(1, &packet(16384, 4), 2);
        feed.restart();
        output.refill();
        assert_eq!(output.next_frame(&mut player), [0.; 2]);
        feed.push(0, &packet(-16384, 4), 1);
        output.refill();
        assert_eq!(output.next_frame(&mut player), [-0.5; 2]);
    }

    #[test]
    fn sync_adds_to_the_target_and_takes_it_back() {
        let (_feed, mut output, _errors) = super::channel(2880, 0);
        let player = Player::new(48000, 48000);
        output.metrics.sync_frames.store(960, std::sync::atomic::Ordering::Relaxed);
        output.prepare_callback(&player, 480);
        assert_eq!(output.jitter.target, 2880 + 960);
        output.metrics.sync_frames.store(0, std::sync::atomic::Ordering::Relaxed);
        output.prepare_callback(&player, 480);
        assert_eq!(output.jitter.target, 2880);
    }

    #[test]
    fn a_large_sync_has_room_and_is_not_trimmed() {
        // 60 ms buffer, then --sync asks for 200 ms more (a Bluetooth speaker next to HDMI).
        let (mut feed, mut output, _errors) = super::channel(2880, 48_000 / 2);
        let mut player = Player::new(48000, 48000);
        output.metrics.sync_frames.store(9600, std::sync::atomic::Ordering::Relaxed);
        output.prepare_callback(&player, 960);
        assert_eq!(output.jitter.target, 2880 + 9600);
        // 300 ms arrive at once: kept (below 4× the target), nothing trimmed.
        for n in 0..15 {
            feed.push(n, &packet(100, 960), 2);
        }
        output.refill();
        player.next_frame(&mut output.jitter);
        assert_eq!(output.jitter.stats.trimmed, 0);
    }

    #[test]
    fn without_room_the_limits_are_as_before() {
        let (_feed, mut output, _errors) = super::channel(2880, 0);
        let player = Player::new(48000, 48000);
        output.prepare_callback(&player, 960);
        assert_eq!((output.jitter.target, output.jitter.max), (2880, 2880 * 4));
    }

    #[test]
    fn short_latency_covers_a_larger_hardware_period() {
        let (mut feed, mut output, _errors) = super::channel(960, 0); // 20 ms
        let mut player = Player::new(48000, 48000);
        for n in 0..3 {
            feed.push(n, &packet(100, 960), 2);
        }
        output.prepare_callback(&player, 1920); // 40-ms hardware period
        output.refill();
        for _ in 0..1920 {
            assert!(output.next_frame(&mut player)[0] > 0.);
        }
        assert_eq!(output.jitter.stats.underruns, 0);
    }
}
