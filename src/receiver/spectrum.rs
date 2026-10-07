//! The spectrum of what arrives, for a program's visualizer (`--json`): 64 bands from 30 Hz to
//! 16 kHz, each 0 to 1 like a browser's AnalyserNode (-100 dB to -30 dB).

use std::f32::consts::PI;

/// Samples looked at: 2048 at 48 kHz is about 43 ms, with bands 23 Hz apart.
const SIZE: usize = 2048;
pub(super) const BANDS: usize = 64;

pub(super) struct Spectrum {
    rate: u32,
    /// The latest SIZE samples (left and right mixed), oldest first from `next`.
    ring: Vec<f32>,
    next: usize,
    window: Vec<f32>,
}

impl Spectrum {
    pub(super) fn new(rate: u32) -> Self {
        let window = (0..SIZE).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / SIZE as f32).cos()).collect();
        Spectrum { rate, ring: vec![0.0; SIZE], next: 0, window }
    }

    /// Big-endian 16-bit PCM as the receiver gets it.
    pub(super) fn feed(&mut self, pcm: &[u8], channels: usize) {
        for frame in pcm.chunks_exact(2 * channels) {
            let sum: f32 = frame.chunks_exact(2).map(|b| f32::from(i16::from_be_bytes([b[0], b[1]]))).sum();
            self.ring[self.next] = sum / (32768.0 * channels as f32);
            self.next = (self.next + 1) % SIZE;
        }
    }

    pub(super) fn bands(&self) -> [f32; BANDS] {
        let mut re: Vec<f32> = (0..SIZE).map(|i| self.ring[(self.next + i) % SIZE] * self.window[i]).collect();
        let mut im = vec![0.0; SIZE];
        fft(&mut re, &mut im);
        // A full-scale sine peaks at SIZE / 4 through the Hann window: that is 0 dB.
        let magnitude = |bin: usize| (re[bin] * re[bin] + im[bin] * im[bin]).sqrt() / (SIZE as f32 / 4.0);
        let step = self.rate as f32 / SIZE as f32;
        let mut bands = [0.0; BANDS];
        for (i, band) in bands.iter_mut().enumerate() {
            let low = 30.0 * (16000.0f32 / 30.0).powf(i as f32 / BANDS as f32);
            let high = 30.0 * (16000.0f32 / 30.0).powf((i + 1) as f32 / BANDS as f32);
            let first = ((low / step).ceil() as usize).max(1);
            let last = ((high / step).ceil() as usize).clamp(first + 1, SIZE / 2);
            let loudest = (first..last).map(magnitude).fold(0.0, f32::max);
            *band = ((20.0 * loudest.max(1e-6).log10() + 100.0) / 70.0).clamp(0.0, 1.0);
        }
        bands
    }
}

/// In-place radix-2 FFT; the length is a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -2.0 * PI / len as f32;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (w_re, w_im) = ((angle * k as f32).cos(), (angle * k as f32).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let (t_re, t_im) = (re[b] * w_re - im[b] * w_im, re[b] * w_im + im[b] * w_re);
                (re[b], im[b]) = (re[a] - t_re, im[a] - t_im);
                (re[a], im[a]) = (re[a] + t_re, im[a] + t_im);
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tone_lights_its_band() {
        let mut spectrum = super::Spectrum::new(48_000);
        let pcm: Vec<u8> = (0..4096).flat_map(|i| (((i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 16000.0) as i16).to_be_bytes()).collect();
        spectrum.feed(&pcm, 1);
        let bands = spectrum.bands();
        let loudest = (0..super::BANDS).max_by(|&a, &b| bands[a].total_cmp(&bands[b])).unwrap();
        // 1 kHz is in band 64·log(1000/30)/log(16000/30) ≈ 35.
        assert!((34..=36).contains(&loudest), "loudest band {loudest}");
        assert!(bands[loudest] > 0.9 && bands[5] < 0.3, "{bands:?}");
    }
}
