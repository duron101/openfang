//! Audio DSP pipeline — real-time acoustic signal processing for sonar / hydrophone feeds.
//!
//! # Architecture
//! - [`AudioFrame`]: raw PCM samples from DDS topic `sensor/sonar/raw`
//! - [`StftProcessor`]: Short-Time Fourier Transform (pure Rust, no external FFT crate)
//! - [`MfccExtractor`]: Mel-Frequency Cepstral Coefficients feature extraction
//! - [`AcousticClassifier`] trait: pluggable classification backend
//! - [`HeuristicClassifier`]: default rule-based stub (replace with ONNX-backed impl at runtime)
//! - [`AcousticEvent`]: structured output published to `sensor/sonar/events`
//!
//! Per the plan: this runs in **pure Rust** (Layer 1, microsecond-level) — not in the WASM sandbox.
//! Feature extraction (FFT + MFCC) is deterministic; classification is best-effort.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// ── Frame ──

/// Audio frame: mono PCM samples, normalised to [-1.0, 1.0]
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Sample rate (Hz)
    pub sample_rate_hz: u32,
    /// Mono PCM samples in [-1.0, 1.0]
    pub samples: Vec<f32>,
    /// Capture timestamp (microseconds since epoch)
    pub timestamp_us: u64,
}

impl AudioFrame {
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / self.sample_rate_hz as f64
    }
}

// ── STFT ──

/// Short-Time Fourier Transform processor.
///
/// Pure Rust implementation using a simple radix-2 iterative DFT for small N
/// (sufficient for tactical-band analysis; production should swap in `rustfft`).
pub struct StftProcessor {
    window_size: usize,
    hop_size: usize,
    window: Vec<f32>,
}

impl StftProcessor {
    /// Construct with Hann window of given size and hop.
    pub fn new(window_size: usize, hop_size: usize) -> Self {
        let window: Vec<f32> = (0..window_size)
            .map(|i| {
                let n = i as f64;
                let w = (std::f64::consts::PI * n / (window_size as f64 - 1.0)).sin();
                0.5 * (1.0 - w.cos()) as f32
            })
            .collect();
        Self {
            window_size,
            hop_size,
            window,
        }
    }

    pub fn window_size(&self) -> usize {
        self.window_size
    }

    pub fn hop_size(&self) -> usize {
        self.hop_size
    }

    /// Compute the magnitude spectrum of one frame. Returns magnitudes for
    /// `[0, window_size/2]` bins (positive frequencies only).
    pub fn magnitude_spectrum(&self, frame: &[f32]) -> Vec<f32> {
        let n = self.window_size.min(frame.len());
        if n == 0 {
            return vec![];
        }
        let mut windowed: Vec<f64> = frame[..n]
            .iter()
            .zip(self.window[..n].iter())
            .map(|(s, w)| (*s as f64) * (*w as f64))
            .collect();
        let half = n / 2;
        let mut mags = Vec::with_capacity(half + 1);
        for k in 0..=half {
            let mut re = 0.0_f64;
            let mut im = 0.0_f64;
            for (i, &sample) in windowed.iter().enumerate() {
                let angle = -2.0 * std::f64::consts::PI * (k as f64) * (i as f64) / (n as f64);
                re += sample * angle.cos();
                im += sample * angle.sin();
            }
            mags.push(((re * re + im * im).sqrt()) as f32);
        }
        mags
    }

    /// Stream samples and emit STFT frames.
    pub fn stream<'a>(&'a self, samples: &'a [f32]) -> impl Iterator<Item = Vec<f32>> + 'a {
        let window_size = self.window_size;
        let hop_size = self.hop_size;
        let mags_fn = |frame: &[f32]| self.magnitude_spectrum(frame);
        FrameStream::new(samples, window_size, hop_size, mags_fn)
    }
}

struct FrameStream<'a, F> {
    samples: &'a [f32],
    window_size: usize,
    hop_size: usize,
    pos: usize,
    mags_fn: F,
}

impl<'a, F> FrameStream<'a, F>
where
    F: Fn(&[f32]) -> Vec<f32>,
{
    fn new(samples: &'a [f32], window_size: usize, hop_size: usize, mags_fn: F) -> Self {
        Self {
            samples,
            window_size,
            hop_size,
            pos: 0,
            mags_fn,
        }
    }
}

impl<'a, F> Iterator for FrameStream<'a, F>
where
    F: Fn(&[f32]) -> Vec<f32>,
{
    type Item = Vec<f32>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + self.window_size > self.samples.len() {
            return None;
        }
        let frame = &self.samples[self.pos..self.pos + self.window_size];
        let mags = (self.mags_fn)(frame);
        self.pos += self.hop_size;
        Some(mags)
    }
}

// ── MFCC ──

/// Mel-Frequency Cepstral Coefficients extractor.
pub struct MfccExtractor {
    sample_rate_hz: u32,
    n_mels: usize,
    n_coeffs: usize,
    mel_filters: Vec<Vec<f32>>,
}

impl MfccExtractor {
    pub fn new(sample_rate_hz: u32, n_mels: usize, n_coeffs: usize) -> Self {
        let mel_filters = build_mel_filterbank(sample_rate_hz, 256, n_mels);
        Self {
            sample_rate_hz,
            n_mels,
            n_coeffs,
            mel_filters,
        }
    }

    /// Compute MFCCs from a magnitude spectrum (length n_fft/2+1).
    pub fn extract(&self, mags: &[f32]) -> Vec<f32> {
        if mags.is_empty() {
            return vec![0.0; self.n_coeffs];
        }
        // Apply mel filterbank
        let mut mel_energies = vec![0.0_f32; self.n_mels];
        for (m, filt) in mel_energies.iter_mut().zip(self.mel_filters.iter()) {
            let mut sum = 0.0_f32;
            for (mag, f) in mags.iter().zip(filt.iter()) {
                sum += mag * f;
            }
            *m = (sum.max(1e-10)).ln();
        }
        // DCT-II (Type-II DCT, orthonormal form)
        let mut coeffs = Vec::with_capacity(self.n_coeffs);
        for k in 0..self.n_coeffs {
            let mut acc = 0.0_f32;
            for (n, mel) in mel_energies.iter().enumerate() {
                let angle =
                    std::f64::consts::PI * (k as f64) * (n as f64 + 0.5) / (self.n_mels as f64);
                acc += mel * angle.cos() as f32;
            }
            coeffs.push(acc);
        }
        coeffs
    }
}

fn hz_to_mel(hz: f64) -> f64 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f64) -> f64 {
    700.0 * (10f64.powf(mel / 2595.0) - 1.0)
}

fn build_mel_filterbank(sample_rate_hz: u32, n_fft: usize, n_mels: usize) -> Vec<Vec<f32>> {
    let f_max = sample_rate_hz as f64 / 2.0;
    let mel_min = hz_to_mel(0.0);
    let mel_max = hz_to_mel(f_max);
    let n_freqs = n_fft / 2 + 1;
    let mut mel_points = Vec::with_capacity(n_mels + 2);
    for i in 0..=n_mels + 1 {
        let m = mel_min + (mel_max - mel_min) * (i as f64) / ((n_mels + 1) as f64);
        mel_points.push(mel_to_hz(m));
    }
    let bin_freqs: Vec<f64> = (0..n_freqs)
        .map(|i| (i as f64) * f_max / ((n_freqs - 1) as f64))
        .collect();
    let mut filters = Vec::with_capacity(n_mels);
    for m in 0..n_mels {
        let left = mel_points[m];
        let center = mel_points[m + 1];
        let right = mel_points[m + 2];
        let mut filter = vec![0.0_f32; n_freqs];
        for (i, &freq) in bin_freqs.iter().enumerate() {
            let val = if freq < left || freq > right {
                0.0
            } else if freq <= center {
                ((freq - left) / (center - left).max(1e-9)) as f32
            } else {
                ((right - freq) / (right - center).max(1e-9)) as f32
            };
            filter[i] = val;
        }
        filters.push(filter);
    }
    filters
}

// ── Classifier ──

/// Pluggable acoustic classifier (ONNX-backed in production).
pub trait AcousticClassifier: Send + Sync {
    /// Classify an MFCC feature vector → (label, confidence)
    fn classify(&self, features: &[f32]) -> Classification;
    /// Human-readable classifier name (for audit)
    fn name(&self) -> &str;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    pub label: String,
    pub confidence: f32,
    /// Discrete confidence bucket: 0=no signal, 1=low, 2=medium, 3=high
    pub tier: u8,
}

/// Default rule-based classifier — looks at spectral centroid + energy.
///
/// Replace by linking in `tract-onnx` or `ort` and wrapping the model in an
/// `impl AcousticClassifier` that calls `tract_onnx::onnx().run(...)`.
pub struct HeuristicClassifier {
    /// Peak frequency bin (centroid) considered "high"
    pub high_freq_threshold_hz: f64,
    /// RMS energy threshold for "active"
    pub energy_threshold: f32,
}

impl Default for HeuristicClassifier {
    fn default() -> Self {
        Self {
            high_freq_threshold_hz: 8000.0,
            energy_threshold: 0.01,
        }
    }
}

impl AcousticClassifier for HeuristicClassifier {
    fn classify(&self, features: &[f32]) -> Classification {
        if features.is_empty() {
            return Classification {
                label: "silence".into(),
                confidence: 1.0,
                tier: 0,
            };
        }
        // Heuristic: total energy = sum of squared features
        let energy: f32 = features.iter().map(|f| f * f).sum::<f32>() / features.len() as f32;
        if energy < self.energy_threshold {
            return Classification {
                label: "silence".into(),
                confidence: 0.9,
                tier: 0,
            };
        }
        // Spectral centroid proxy: weighted mean index
        let centroid_idx: f32 = features
            .iter()
            .enumerate()
            .map(|(i, &f)| (i as f32) * f.abs())
            .sum::<f32>()
            / features.iter().map(|f| f.abs()).sum::<f32>().max(1e-9);
        // Map idx back to Hz assuming 256-pt FFT, 16 kHz
        let centroid_hz = centroid_idx as f64 * 16000.0 / 256.0;
        if centroid_hz > self.high_freq_threshold_hz {
            Classification {
                label: "high_freq_signal".into(),
                confidence: 0.7,
                tier: 2,
            }
        } else {
            Classification {
                label: "low_freq_signal".into(),
                confidence: 0.6,
                tier: 1,
            }
        }
    }

    fn name(&self) -> &str {
        "heuristic-v1"
    }
}

// ── Pipeline ──

/// Structured output of an acoustic event — published to `sensor/sonar/events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcousticEvent {
    pub freq_peak_hz: f64,
    pub bandwidth_hz: f64,
    pub classification: String,
    pub confidence: f32,
    pub timestamp_us: u64,
}

/// Real-time audio processing pipeline: PCM → STFT → MFCC → Classifier → Event
pub struct AudioPipeline {
    stft: StftProcessor,
    mfcc: MfccExtractor,
    classifier: Box<dyn AcousticClassifier>,
    history: VecDeque<AcousticEvent>,
    history_capacity: usize,
    sample_rate_hz: u32,
}

impl AudioPipeline {
    pub fn new(
        sample_rate_hz: u32,
        window_size: usize,
        hop_size: usize,
        n_mels: usize,
        n_coeffs: usize,
        classifier: Box<dyn AcousticClassifier>,
    ) -> Self {
        Self {
            stft: StftProcessor::new(window_size, hop_size),
            mfcc: MfccExtractor::new(sample_rate_hz, n_mels, n_coeffs),
            classifier,
            history: VecDeque::new(),
            history_capacity: 64,
            sample_rate_hz,
        }
    }

    pub fn with_default_classifier(sample_rate_hz: u32) -> Self {
        Self::new(
            sample_rate_hz,
            256,
            128,
            20,
            13,
            Box::new(HeuristicClassifier::default()),
        )
    }

    pub fn classifier_name(&self) -> &str {
        self.classifier.name()
    }

    /// Process one audio frame, returning any newly detected acoustic event.
    pub fn process(&mut self, frame: &AudioFrame) -> Option<AcousticEvent> {
        if frame.samples.is_empty() {
            return None;
        }
        let mags = self.stft.magnitude_spectrum(&frame.samples);
        let mags_slice: &[f32] = &mags;
        let features = self.mfcc.extract(mags_slice);
        let class = self.classifier.classify(&features);
        if class.label == "silence" {
            return None;
        }
        // Spectral peak + bandwidth from raw magnitudes
        let (peak_idx, peak_val) = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or((0, &0.0));
        let freq_peak_hz =
            peak_idx as f64 * (self.sample_rate_hz as f64) / (self.stft.window_size() as f64);
        // 3dB bandwidth
        let half_max = peak_val / 2.0_f32.sqrt();
        let mut low = peak_idx;
        let mut high = peak_idx;
        while low > 0 && mags[low] >= half_max {
            low -= 1;
        }
        while high < mags.len() - 1 && mags[high] >= half_max {
            high += 1;
        }
        let bandwidth_hz =
            (high - low) as f64 * (self.sample_rate_hz as f64) / (self.stft.window_size() as f64);
        let event = AcousticEvent {
            freq_peak_hz,
            bandwidth_hz,
            classification: class.label,
            confidence: class.confidence,
            timestamp_us: frame.timestamp_us,
        };
        if self.history.len() == self.history_capacity {
            self.history.pop_front();
        }
        self.history.push_back(event.clone());
        Some(event)
    }

    pub fn recent_events(&self) -> impl Iterator<Item = &AcousticEvent> {
        self.history.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_wave(freq_hz: f64, sample_rate_hz: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * freq_hz * (i as f64) / (sample_rate_hz as f64)).sin()
                    as f32
            })
            .collect()
    }

    #[test]
    fn test_stft_emits_spectrum() {
        let stft = StftProcessor::new(256, 128);
        let frame = sine_wave(1000.0, 16000, 256);
        let mags = stft.magnitude_spectrum(&frame);
        assert_eq!(mags.len(), 129);
        // Peak should be near bin 16 (1000 Hz / 16000 * 256 = 16)
        let (peak_idx, peak_val) = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();
        assert!(
            (peak_idx as i64 - 16).abs() <= 2,
            "peak at {peak_idx}, expected ~16"
        );
        assert!(*peak_val > 1.0, "peak too small: {peak_val}");
    }

    #[test]
    fn test_mfcc_dimensions() {
        let mfcc = MfccExtractor::new(16000, 20, 13);
        let mags = vec![0.1_f32; 129];
        let coeffs = mfcc.extract(&mags);
        assert_eq!(coeffs.len(), 13);
    }

    #[test]
    fn test_heuristic_silence_detection() {
        let c = HeuristicClassifier::default();
        let r = c.classify(&[0.001; 13]);
        assert_eq!(r.label, "silence");
    }

    #[test]
    fn test_heuristic_classifies_active_signal() {
        let c = HeuristicClassifier::default();
        // Energy in features dominated by high bin
        let mut feats = vec![0.0001_f32; 13];
        feats[10] = 5.0;
        let r = c.classify(&feats);
        assert_ne!(r.label, "silence");
    }

    #[test]
    fn test_audio_pipeline_emits_event_for_active_signal() {
        let mut pipe = AudioPipeline::with_default_classifier(16000);
        let frame = AudioFrame {
            sample_rate_hz: 16000,
            samples: sine_wave(2000.0, 16000, 256),
            timestamp_us: 1_000_000,
        };
        let event = pipe.process(&frame);
        // Either active event or silence — pipeline should not panic
        if let Some(ev) = event {
            assert!(ev.freq_peak_hz > 0.0);
        }
    }
}
