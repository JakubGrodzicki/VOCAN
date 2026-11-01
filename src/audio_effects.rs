//! Additional DSP modules for Automixer.
//!
//! All functions operate on interleaved f32 PCM, mono or stereo,
//! at the sample_rate provided as an argument. They return Vec<f32>
//! in the same format.
//!
//! Dependencies in Cargo.toml:
//!   rustfft     = "6.2"
//!   biquad      = "0.4"
//!   nnnoiseless = "0.5"
//!   tempfile    = "3"
//!   hound       = "3.5"

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::types::ReductionProfile;

/// Creates a Command with CREATE_NO_WINDOW on Windows (for non-FFmpeg binaries).
fn silent_command(bin: &Path) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd
}

// ===========================================================================
// MODULE 1: Spectral Gate
// ===========================================================================

pub struct SpectralGateParams {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_s: f32,
    pub release_s: f32,
}

impl Default for SpectralGateParams {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            ratio: 2.0,
            attack_s: 0.002,
            release_s: 0.080,
        }
    }
}

pub fn apply_spectral_gate(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    p: &SpectralGateParams,
) -> Result<Vec<f32>> {
    use rustfft::{num_complex::Complex, FftPlanner};

    if samples.is_empty() {
        return Ok(Vec::new());
    }
    // `frames_total` below divides by `ch`, so a zero channel count panics.
    // `apply_expander_inplace` already treats that as "leave the signal alone"
    // rather than as a hard error; this is a public entry point too, so it
    // mirrors that contract instead of introducing a second one.
    if channels == 0 {
        return Ok(samples.to_vec());
    }

    let fft_size = 2048usize;
    let hop = fft_size / 4;
    let window: Vec<f32> = (0..fft_size)
        .map(|n| {
            let x = n as f32 / (fft_size - 1) as f32;
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
        })
        .collect();

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let ifft = planner.plan_fft_inverse(fft_size);

    // rustfft's `Fft::process` allocates and zeroes a fresh scratch buffer on
    // every single call. At 48kHz with hop=512 that is ~110k allocations for a
    // 10-minute file, so the scratch is hoisted here and reused through
    // `process_with_scratch`.
    let mut scratch = vec![
        Complex { re: 0.0, im: 0.0 };
        fft.get_inplace_scratch_len()
            .max(ifft.get_inplace_scratch_len())
    ];

    let threshold_lin = 10f32.powf(p.threshold_db / 20.0);
    let ratio = p.ratio.max(1.0);
    // Expansion exponent, hoisted out of the per-bin loop. The default ratio of
    // 2.0 makes this exactly 1.0, where the gain is just the level ratio and no
    // transcendental call is needed at all.
    let gate_exp = ratio - 1.0;
    let frame_time = hop as f32 / sample_rate as f32;
    let a_coef = (-frame_time / p.attack_s).exp();
    let r_coef = (-frame_time / p.release_s).exp();

    let ch = channels as usize;
    let frames_total = samples.len() / ch;

    // The STFT runs over a signal padded with `fft_size` zeros at each end.
    // Without that padding the loop below only visits positions where a whole
    // analysis window fits inside the real signal, so the leading
    // `fft_size - hop` samples receive partial overlap-add (an audible
    // fade-in) and everything past the last full frame -- up to `fft_size - 1`
    // samples, ~43ms at 48kHz -- is never written at all and stays silent.
    // Padding puts every real sample in the interior, where the Hann^2 /
    // 75%-overlap COLA sum is constant and `comp` below is the exact inverse.
    let pad = fft_size;
    let padded_len = frames_total + 2 * pad;
    let comp = 2.0 / 3.0;

    // Both buffers below stay `vec![0f32; n]` rather than becoming
    // `try_reserve` + `resize`: that form hits std's `IsZero` specialisation and
    // goes to `alloc_zeroed`, which for a ~100 MB accumulator hands back
    // already-zeroed pages instead of writing over the whole buffer. Since the
    // sizes are known up front, checking them is fallible *and* free, whereas
    // `try_reserve` + `resize` would be fallible at the cost of a full extra
    // write pass over the largest buffer in the pipeline.
    crate::memory::check_alloc(padded_len, 4, "the spectral gate accumulator")?;
    if ch != 1 {
        crate::memory::check_alloc(samples.len(), 4, "the spectral gate output buffer")?;
    }

    // Mono reuses the per-channel accumulator as the return buffer, so only the
    // multi-channel path needs a separate interleaved output up front.
    let mut out = if ch == 1 {
        Vec::new()
    } else {
        vec![0f32; samples.len()]
    };

    let mut buf: Vec<Complex<f32>> = vec![Complex { re: 0.0, im: 0.0 }; fft_size];

    for c in 0..ch {
        let mut acc = vec![0f32; padded_len];
        let mut bin_env = vec![0f32; fft_size / 2 + 1];

        let mut pos = 0usize;
        while pos + fft_size <= padded_len {
            for n in 0..fft_size {
                // `pos + n` indexes the padded signal; shift back onto the real
                // one. Positions inside either pad wrap to a huge value and so
                // fail the bounds check, yielding the intended zero.
                let src = (pos + n).wrapping_sub(pad);
                let s = if src < frames_total {
                    samples[src * ch + c]
                } else {
                    0.0
                };
                buf[n].re = s * window[n];
                buf[n].im = 0.0;
            }
            fft.process_with_scratch(&mut buf, &mut scratch);

            for k in 0..=fft_size / 2 {
                let mag = (buf[k].re * buf[k].re + buf[k].im * buf[k].im).sqrt();
                let target = mag;
                let env = &mut bin_env[k];
                let coef = if target > *env { a_coef } else { r_coef };
                *env = target + coef * (*env - target);

                let gain = if *env < threshold_lin {
                    let x = *env / threshold_lin;
                    // `powf(x, gate_exp)` without its generic slow path. For
                    // x == 0.0 this is exp2(-inf) == 0.0, matching powf; the
                    // max() still guards against a NaN reaching the output.
                    let g = if gate_exp == 1.0 {
                        x
                    } else {
                        (gate_exp * x.log2()).exp2()
                    };
                    g.max(0.0)
                } else {
                    1.0
                };
                buf[k].re *= gain;
                buf[k].im *= gain;
                if k > 0 && k < fft_size / 2 {
                    buf[fft_size - k] = Complex {
                        re: buf[k].re,
                        im: -buf[k].im,
                    };
                }
            }

            ifft.process_with_scratch(&mut buf, &mut scratch);
            let norm = 1.0 / fft_size as f32;
            for n in 0..fft_size {
                acc[pos + n] += buf[n].re * norm * window[n];
            }
            pos += hop;
        }

        if ch == 1 {
            // Shift the useful region down over the leading pad and drop the
            // trailing pad in place, rather than allocating a second
            // full-length buffer just to copy into.
            acc.copy_within(pad..pad + frames_total, 0);
            acc.truncate(frames_total);
            for v in acc.iter_mut() {
                *v *= comp;
            }
            out = acc;
        } else {
            for i in 0..frames_total {
                out[i * ch + c] = acc[pad + i] * comp;
            }
        }
    }
    Ok(out)
}

// ===========================================================================
// MODULE 2: Adaptive voice EQ (measure the take, then four gentle static gains)
// ===========================================================================
//
// Compares the long-term spectrum of the speech in a take with the long-term
// spectrum of speech in general, in four wide bands, and moves each band a
// little of the way towards it: at most +/-1.5 dB overall.
//
// The gains are settled once per file and do not move while it plays. That is
// what keeps the phase constant and rules out pumping, and it is why this can be
// a handful of wide minimum-phase biquads instead of a time-varying filter bank.
//
// The reference is a speech spectrum, not a flat line: "evening out" a voice
// towards flat is how you end up with something that sounds like white noise.

/// ANSI S3.5 (SII) standard speech spectrum level, normal vocal effort:
/// third-octave centre frequency in Hz, spectrum level in dB per Hz. Values as
/// published in the open-source MoSQITo implementation of the standard.
///
/// A *density*, which matters: see [`speech_target_db`] for the conversion to
/// band power. A provisional reference for close-miked voice-over, kept in this
/// one table so calibrating it against real takes means changing only this.
const SPEECH_SPECTRUM: [(f32, f32); 18] = [
    (160.0, 32.41),
    (200.0, 34.48),
    (250.0, 34.75),
    (315.0, 33.98),
    (400.0, 34.59),
    (500.0, 34.27),
    (630.0, 32.06),
    (800.0, 28.30),
    (1000.0, 25.01),
    (1250.0, 23.00),
    (1600.0, 20.15),
    (2000.0, 17.32),
    (2500.0, 13.18),
    (3150.0, 11.55),
    (4000.0, 9.33),
    (5000.0, 5.31),
    (6300.0, 2.59),
    (8000.0, 1.13),
];

/// How many bands the EQ measures and corrects.
pub const EQ_BANDS: usize = 4;
/// Short names of the bands, for the log.
const EQ_BAND_NAMES: [&str; EQ_BANDS] = ["low", "low-mid", "presence", "high"];
/// Which [`SPEECH_SPECTRUM`] rows make up each band (start, end).
const EQ_BAND_ROWS: [(usize, usize); EQ_BANDS] = [(0, 2), (2, 8), (8, 15), (15, 18)];
/// The largest change the whole EQ may make at any frequency, in dB.
const EQ_LIMIT_DB: f32 = 1.5;
/// How much of the measured deviation a band is corrected by.
const EQ_STRENGTH: f32 = 0.5;
/// A band this far from the shape of the others is cut off or empty rather than
/// merely coloured. A gentle boost cannot fix that, it can only raise the noise.
const EQ_OUTLIER_DB: f32 = 12.0;
/// A band is only boosted if the speech in it is this far above the background.
const EQ_MIN_SNR_DB: f32 = 12.0;
/// Below this much speech the take says more about which sounds happen to be in
/// the line than about the recording, so nothing is corrected.
const EQ_MIN_SPEECH_SECS: f32 = 1.0;
/// From this much speech on, a measured deviation is trusted in full.
const EQ_FULL_SPEECH_SECS: f32 = 4.0;
/// Frames this far below the mean level of the file count as background, and
/// everything above as speech. One gate with no neutral zone between the two:
/// the reference is an average over all of running speech, and the unvoiced
/// consonants that carry much of the top band sit well under the mean. A
/// stricter speech gate would drop them, read the top band as dull, and boost it
/// on nearly every file.
const EQ_BACKGROUND_GATE_DB: f32 = -25.0;
/// Where the noise check on the top band stops, if Nyquist does not stop it first.
const EQ_TOP_GUARD_HZ: f32 = 20_000.0;
/// A frame below this energy is digital silence (-140 dBFS RMS, the same floor
/// [`estimate_noise_floor_db`] uses).
const EQ_SILENT_ENERGY: f32 = 1e-14;

/// The edges of a third-octave band centred on `centre`.
fn third_octave_edges(centre: f32) -> (f32, f32) {
    let half_band = 2f32.powf(1.0 / 6.0);
    (centre / half_band, centre * half_band)
}

/// The [`SPEECH_SPECTRUM`] rows of `band` that fit under `nyquist`.
///
/// Measurement and target both go through this, so a band cut short by a low
/// sample rate is shortened on both sides of the comparison.
fn eq_band_rows(band: usize, nyquist: f32) -> &'static [(f32, f32)] {
    let (start, end) = EQ_BAND_ROWS[band];
    let rows = &SPEECH_SPECTRUM[start..end];
    let fit = rows
        .iter()
        .take_while(|(centre, _)| third_octave_edges(*centre).1 <= nyquist)
        .count();
    &rows[..fit]
}

/// The total power of each band in the reference spectrum, in dB, or `None` for
/// a band entirely above `nyquist`.
///
/// Total power, the same quantity [`measure_voice_bands`] sums: a density
/// compared against a total would drift with the width of the band, and these
/// bands differ in width by more than a factor of 40.
fn speech_target_db(nyquist: f32) -> [Option<f32>; EQ_BANDS] {
    std::array::from_fn(|band| {
        let rows = eq_band_rows(band, nyquist);
        if rows.is_empty() {
            return None;
        }
        let power: f64 = rows
            .iter()
            .map(|&(centre, level)| {
                let (lo, hi) = third_octave_edges(centre);
                10f64.powf(level as f64 / 10.0) * (hi - lo) as f64
            })
            .sum();
        Some(10.0 * power.log10() as f32)
    })
}

/// What [`measure_voice_bands`] found in a take.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceBands {
    /// Mean power of the speech frames per band, in dB (arbitrary reference);
    /// `None` for a band above Nyquist.
    pub speech_db: [Option<f32>; EQ_BANDS],
    /// How far the speech is above the background in each band, in dB.
    ///
    /// The top band is judged up to [`EQ_TOP_GUARD_HZ`] rather than over its
    /// measured range: its shelf lifts everything above it, and that is where
    /// residual hiss lives. The low shelf has the same gap below its band, but
    /// the fixed high-pass that follows covers most of that.
    pub snr_db: [Option<f32>; EQ_BANDS],
    /// How much of the take counted as speech.
    pub speech_secs: f32,
}

/// Measures the spectrum of the speech and of the background in a mono signal.
///
/// Speech is told from background by frame energy relative to the mean level of
/// the file, the way R128 gates loudness. Silero VAD would be more exact, but it
/// is optional and costs a process per file. A take cut so tightly that nothing
/// falls under the gate uses its quietest 12% as the background instead, as
/// [`estimate_noise_floor_db`] does; that understates the signal-to-noise ratio,
/// which only makes boosts less likely.
///
/// Returns `None` when there is nothing to measure: the signal is shorter than
/// one frame or digitally silent throughout.
pub fn measure_voice_bands(samples: &[f32], sample_rate: u32) -> Option<VoiceBands> {
    use rustfft::{num_complex::Complex, FftPlanner};

    if sample_rate == 0 {
        return None;
    }
    let fft_size = ((sample_rate as f32 * 0.04) as usize)
        .next_power_of_two()
        .max(16);
    let hop = fft_size / 2;
    if samples.len() < fft_size {
        return None;
    }
    let window: Vec<f32> = (0..fft_size)
        .map(|n| {
            let x = n as f32 / (fft_size - 1) as f32;
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
        })
        .collect();
    let inv_w2_sum = 1.0 / window.iter().map(|w| w * w).sum::<f32>();

    let energies: Vec<f32> = samples
        .windows(fft_size)
        .step_by(hop)
        .map(|frame| {
            let sum: f32 = frame
                .iter()
                .zip(&window)
                .map(|(s, w)| (s * w) * (s * w))
                .sum();
            sum * inv_w2_sum
        })
        .collect();

    let mut audible: Vec<f32> = energies
        .iter()
        .copied()
        .filter(|e| *e > EQ_SILENT_ENERGY)
        .collect();
    if audible.is_empty() {
        return None;
    }
    let mean = audible.iter().map(|e| *e as f64).sum::<f64>() / audible.len() as f64;
    let speech_floor = (mean * 10f64.powf(EQ_BACKGROUND_GATE_DB as f64 / 10.0)) as f32;
    let mut background_ceiling = speech_floor;
    // Digital silence counts as background: a take with zeroed pauses has a
    // background, and it is a very quiet one.
    if !energies.iter().any(|e| *e < background_ceiling) {
        let idx = ((audible.len() as f32 * 0.12) as usize).min(audible.len() - 1);
        let (_, quiet, _) = audible.select_nth_unstable_by(idx, f32::total_cmp);
        // Just above it, because the comparison below is a strict one.
        background_ceiling = *quiet * 1.000_001;
    }

    // Each bin counts towards a band by how much of its width falls inside the
    // band. At 48 kHz a bin is 23 Hz and the lowest band only 82 Hz wide, so
    // rounding to whole bins would bias that band by up to a decibel.
    let nyquist = sample_rate as f32 / 2.0;
    let bin_hz = sample_rate as f32 / fft_size as f32;
    let weights_between = |lo: f32, hi: f32| -> Vec<f32> {
        (0..=fft_size / 2)
            .map(|k| {
                let centre = k as f32 * bin_hz;
                let overlap = (centre + bin_hz / 2.0).min(hi) - (centre - bin_hz / 2.0).max(lo);
                (overlap / bin_hz).max(0.0)
            })
            .collect()
    };
    let weights: [Vec<f32>; EQ_BANDS] = std::array::from_fn(|band| {
        let rows = eq_band_rows(band, nyquist);
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
            return Vec::new();
        };
        weights_between(third_octave_edges(first.0).0, third_octave_edges(last.0).1)
    });
    let top = EQ_BANDS - 1;
    let top_guard = match eq_band_rows(top, nyquist).first() {
        Some(first) => weights_between(third_octave_edges(first.0).0, nyquist.min(EQ_TOP_GUARD_HZ)),
        None => Vec::new(),
    };

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut scratch = vec![Complex { re: 0.0, im: 0.0 }; fft.get_inplace_scratch_len()];
    let mut buf = vec![Complex { re: 0.0, im: 0.0 }; fft_size];

    let mut speech_power = [0f64; EQ_BANDS];
    let mut background_power = [0f64; EQ_BANDS];
    let (mut speech_top_guard, mut background_top_guard) = (0f64, 0f64);
    let (mut speech_frames, mut background_frames) = (0usize, 0usize);

    for (i, &energy) in energies.iter().enumerate() {
        let is_speech = energy >= speech_floor;
        let is_background = energy < background_ceiling;
        if !is_speech && !is_background {
            continue;
        }
        let frame = &samples[i * hop..i * hop + fft_size];
        for ((slot, s), w) in buf.iter_mut().zip(frame).zip(&window) {
            *slot = Complex { re: s * w, im: 0.0 };
        }
        fft.process_with_scratch(&mut buf, &mut scratch);

        let power_in = |weights: &[f32]| -> f64 {
            weights
                .iter()
                .zip(&buf)
                .map(|(w, x)| *w as f64 * (x.re * x.re + x.im * x.im) as f64)
                .sum()
        };
        for band in 0..EQ_BANDS {
            let power = power_in(&weights[band]);
            if is_speech {
                speech_power[band] += power;
            }
            if is_background {
                background_power[band] += power;
            }
        }
        let guard_power = power_in(&top_guard);
        if is_speech {
            speech_top_guard += guard_power;
        }
        if is_background {
            background_top_guard += guard_power;
        }
        speech_frames += usize::from(is_speech);
        background_frames += usize::from(is_background);
    }

    let to_db = |power: f64, frames: usize, band: usize| {
        (!weights[band].is_empty())
            .then(|| 10.0 * (power / frames.max(1) as f64).max(1e-20).log10() as f32)
    };
    Some(VoiceBands {
        speech_db: std::array::from_fn(|b| to_db(speech_power[b], speech_frames, b)),
        snr_db: std::array::from_fn(|b| {
            let (speech, background) = if b == top {
                (speech_top_guard, background_top_guard)
            } else {
                (speech_power[b], background_power[b])
            };
            let speech = to_db(speech, speech_frames, b)?;
            Some(speech - to_db(background, background_frames, b)?)
        }),
        speech_secs: speech_frames as f32 * hop as f32 / sample_rate as f32,
    })
}

/// Why a band got the gain it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandDecision {
    /// Corrected from the measurement (the gain may still round to nothing).
    Corrected,
    /// The band lies above the file's Nyquist frequency.
    AboveNyquist,
    /// The band is too far from the shape of the others to be a matter of
    /// colour: cut off, or empty.
    NoUsableContent,
    /// It wanted a boost, but that would raise the noise as much as the voice.
    TooNoisyToBoost,
}

/// Why the whole stage was left out for a file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EqBypass {
    /// Too little speech for the measurement to say anything about the recording.
    TooLittleSpeech,
    /// Too few usable bands (see [`plan_adaptive_eq`]): no shape left to compare.
    TooFewBands,
}

/// The outcome of [`plan_adaptive_eq`].
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptiveEqPlan {
    pub gains_db: [f32; EQ_BANDS],
    /// How far each band measured from the reference, level taken out, in dB:
    /// positive means hotter than the reference. Raw, before the strength,
    /// confidence and limits that turn it into a gain.
    pub shape_db: [Option<f32>; EQ_BANDS],
    pub decisions: [BandDecision; EQ_BANDS],
    pub bypass: Option<EqBypass>,
    pub speech_secs: f32,
}

impl AdaptiveEqPlan {
    /// One line for the log, and what calibrating [`SPEECH_SPECTRUM`] is done
    /// from. It carries the measured shape as well as the gain because a gain at
    /// the limit could mean 3 dB off or 20 dB off: a band that measures off in
    /// one direction across a whole batch means the reference is off for this
    /// material.
    pub fn describe(&self) -> String {
        match self.bypass {
            Some(EqBypass::TooLittleSpeech) => {
                return format!(
                    "adaptive EQ left out, only {:.1} s of speech to measure",
                    self.speech_secs
                );
            }
            Some(EqBypass::TooFewBands) => {
                return "adaptive EQ left out, the spectrum is too far from speech to compare"
                    .to_owned();
            }
            None => {}
        }
        let bands: Vec<String> = (0..EQ_BANDS)
            .map(|b| {
                let name = EQ_BAND_NAMES[b];
                let shape = self.shape_db[b].unwrap_or(0.0);
                match self.decisions[b] {
                    BandDecision::Corrected => {
                        format!("{name} {:+.1} dB (measured {shape:+.1})", self.gains_db[b])
                    }
                    BandDecision::AboveNyquist => format!("{name} n/a"),
                    BandDecision::NoUsableContent => {
                        format!("{name} 0 dB, no usable content (measured {shape:+.1})")
                    }
                    BandDecision::TooNoisyToBoost => {
                        format!("{name} 0 dB, too noisy to boost (measured {shape:+.1})")
                    }
                }
            })
            .collect();
        format!(
            "adaptive EQ {}, from {:.1} s of speech",
            bands.join(", "),
            self.speech_secs
        )
    }
}

/// Decides the gain of each band. No I/O and no audio, so every rule below can
/// be tested on made-up measurements.
pub fn plan_adaptive_eq(bands: &VoiceBands, sample_rate: u32) -> AdaptiveEqPlan {
    let mut plan = AdaptiveEqPlan {
        gains_db: [0.0; EQ_BANDS],
        shape_db: [None; EQ_BANDS],
        decisions: [BandDecision::AboveNyquist; EQ_BANDS],
        bypass: None,
        speech_secs: bands.speech_secs,
    };

    let target = speech_target_db(sample_rate as f32 / 2.0);
    let devs: [Option<f32>; EQ_BANDS] =
        std::array::from_fn(|b| Some(target[b]? - bands.speech_db[b]?));

    // The raw deviations carry the absolute difference between dBFS and the
    // table, which is tens of dB and means nothing. The median takes that out
    // before outliers are judged, and one cut-off band cannot drag it along.
    let mut sorted: Vec<f32> = devs.iter().flatten().copied().collect();
    sorted.sort_by(f32::total_cmp);
    let median = match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    };

    let mut kept: Vec<f32> = Vec::with_capacity(EQ_BANDS);
    for (b, dev) in devs.iter().enumerate() {
        if let Some(dev) = *dev {
            if (dev - median).abs() > EQ_OUTLIER_DB {
                plan.decisions[b] = BandDecision::NoUsableContent;
            } else {
                plan.decisions[b] = BandDecision::Corrected;
                kept.push(dev);
            }
        }
    }
    // A majority, not just two: with two of four bands out, the median sits
    // between two unrelated groups and "the shape of the others" means nothing.
    // A steady tone looks exactly like that, and so does a telephone band.
    if kept.len() < 2 || kept.len() * 2 <= sorted.len() {
        plan.bypass = Some(EqBypass::TooFewBands);
        return plan;
    }
    if bands.speech_secs < EQ_MIN_SPEECH_SECS {
        plan.bypass = Some(EqBypass::TooLittleSpeech);
        return plan;
    }

    // Only the shape is corrected; loudness normalization sets the level.
    let offset = kept.iter().sum::<f32>() / kept.len() as f32;
    let confidence = ((bands.speech_secs - EQ_MIN_SPEECH_SECS)
        / (EQ_FULL_SPEECH_SECS - EQ_MIN_SPEECH_SECS))
        .clamp(0.0, 1.0);
    plan.shape_db = devs.map(|dev| dev.map(|dev| offset - dev));

    for (b, dev) in devs.iter().enumerate() {
        if plan.decisions[b] != BandDecision::Corrected {
            continue;
        }
        let Some(dev) = *dev else { continue };
        let gain = (EQ_STRENGTH * confidence * (dev - offset)).clamp(-EQ_LIMIT_DB, EQ_LIMIT_DB);
        let snr = bands.snr_db[b].unwrap_or(f32::INFINITY);
        if gain > 0.0 && snr < EQ_MIN_SNR_DB {
            plan.decisions[b] = BandDecision::TooNoisyToBoost;
        } else {
            plan.gains_db[b] = gain;
        }
    }

    // The filters overlap, so neighbours within the limit can exceed it together.
    // Scaling is not exactly linear in dB, hence a few rounds.
    for _ in 0..4 {
        let peak = eq_response_peak_db(&plan.gains_db, sample_rate);
        if peak <= EQ_LIMIT_DB {
            break;
        }
        let scale = EQ_LIMIT_DB / peak;
        plan.gains_db.iter_mut().for_each(|g| *g *= scale);
    }
    plan
}

/// The biquads for `gains_db`: one wide filter per band, or `None` for a band
/// with nothing to do or whose frequency a low sample rate cannot represent.
fn eq_filters(
    gains_db: &[f32; EQ_BANDS],
    sample_rate: u32,
) -> [Option<biquad::Coefficients<f32>>; EQ_BANDS] {
    use biquad::{Coefficients, ToHertz, Type, Q_BUTTERWORTH_F32};

    let designs: [(Type<f32>, f32, f32); EQ_BANDS] = [
        (Type::LowShelf(gains_db[0]), 225.0, Q_BUTTERWORTH_F32),
        (Type::PeakingEQ(gains_db[1]), 450.0, 0.67),
        (Type::PeakingEQ(gains_db[2]), 2000.0, 0.55),
        (Type::HighShelf(gains_db[3]), 4500.0, Q_BUTTERWORTH_F32),
    ];
    let fs = (sample_rate as f32).hz();
    std::array::from_fn(|b| {
        if gains_db[b].abs() < 0.01 {
            return None;
        }
        let (kind, freq, q) = designs[b];
        Coefficients::<f32>::from_params(kind, fs, freq.hz(), q).ok()
    })
}

/// The combined response of the filters at `freq`, in dB.
fn eq_response_db(
    filters: &[Option<biquad::Coefficients<f32>>; EQ_BANDS],
    freq: f32,
    sample_rate: u32,
) -> f32 {
    let w = 2.0 * std::f64::consts::PI * freq as f64 / sample_rate as f64;
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    filters
        .iter()
        .flatten()
        .map(|f| {
            let (b0, b1, b2) = (f.b0 as f64, f.b1 as f64, f.b2 as f64);
            let (a1, a2) = (f.a1 as f64, f.a2 as f64);
            // |H|^2 at e^(jw): z^-1 = cos w - j sin w, z^-2 = cos 2w - j sin 2w.
            let num = (b0 + b1 * c1 + b2 * c2).powi(2) + (b1 * s1 + b2 * s2).powi(2);
            let den = (1.0 + a1 * c1 + a2 * c2).powi(2) + (a1 * s1 + a2 * s2).powi(2);
            (10.0 * (num / den).log10()) as f32
        })
        .sum()
}

/// The largest absolute response of the filters for `gains_db`, in dB, across
/// the audible range below Nyquist.
fn eq_response_peak_db(gains_db: &[f32; EQ_BANDS], sample_rate: u32) -> f32 {
    const POINTS: usize = 96;
    let filters = eq_filters(gains_db, sample_rate);
    let top = (sample_rate as f32 * 0.49).min(20_000.0);
    (0..POINTS)
        .map(|i| 20.0 * (top / 20.0).powf(i as f32 / (POINTS - 1) as f32))
        .map(|freq| eq_response_db(&filters, freq, sample_rate).abs())
        .fold(0.0, f32::max)
}

/// Applies the gains of an [`AdaptiveEqPlan`] in place.
///
/// Infallible: a band with no gain or no room below Nyquist simply has no
/// filter, so all-zero gains leave the signal untouched to the bit.
pub fn apply_adaptive_eq_inplace(
    buf: &mut [f32],
    sample_rate: u32,
    channels: u16,
    gains_db: &[f32; EQ_BANDS],
) {
    use biquad::{Biquad, DirectForm2Transposed};

    // Same guard as `apply_spectral_gate` / `apply_expander_inplace`: `frames`
    // below divides by `ch`, which panics at zero.
    if buf.is_empty() || channels == 0 {
        return;
    }
    let coeffs: Vec<_> = eq_filters(gains_db, sample_rate)
        .into_iter()
        .flatten()
        .collect();
    if coeffs.is_empty() {
        return;
    }

    let ch = channels as usize;
    let frames = buf.len() / ch;
    for c in 0..ch {
        let mut filters: Vec<DirectForm2Transposed<f32>> = coeffs
            .iter()
            .map(|co| DirectForm2Transposed::<f32>::new(*co))
            .collect();
        for i in 0..frames {
            let idx = i * ch + c;
            let mut x = buf[idx];
            for f in filters.iter_mut() {
                x = <DirectForm2Transposed<f32> as Biquad<f32>>::run(f, x);
            }
            buf[idx] = x;
        }
    }
}

// ===========================================================================
// MODULE 3: Denoise via nnnoiseless (with mix and latency compensation)
// ===========================================================================
//
// Expects 48 kHz mono, f32 in range ±1.0.

pub struct NnnoiseParams {
    /// Dry/wet mix: 0.0 = bypass only, 1.0 = wet only (full denoise).
    /// Recommended 0.6–0.8 for speech — minimizes artifacts on breaths.
    pub mix: f32,
}

impl Default for NnnoiseParams {
    fn default() -> Self {
        Self { mix: 0.75 }
    }
}

pub fn apply_nnnoise(samples_48k_mono: &[f32], params: &NnnoiseParams) -> Result<Vec<f32>> {
    use nnnoiseless::DenoiseState;

    let mut state = DenoiseState::new();
    let frame_size = DenoiseState::FRAME_SIZE;
    let mix = params.mix.clamp(0.0, 1.0);
    let dry_len = samples_48k_mono.len();

    // Seed the output with the dry contribution and mix each denoised frame in
    // on top as it is produced. Peak memory is one output buffer plus two
    // frame-sized scratch buffers; the previous version held three
    // full-length copies of the signal (scaled + wet + out) at once, on top of
    // the caller's own buffer.
    let mut out: Vec<f32> = samples_48k_mono
        .iter()
        .map(|&dry| dry * (1.0 - mix))
        .collect();

    // Reusable per-frame buffers — no per-frame allocation.
    let mut input_buf = vec![0f32; frame_size];
    let mut frame_buf = vec![0f32; frame_size];

    let total_frames = dry_len.div_ceil(frame_size);
    for f in 0..total_frames {
        let start = f * frame_size;
        let end = (start + frame_size).min(dry_len);
        let len = end - start;
        for (dst, &x) in input_buf[..len]
            .iter_mut()
            .zip(&samples_48k_mono[start..end])
        {
            *dst = (x * 32768.0).clamp(-32768.0, 32767.0);
        }
        input_buf[len..].fill(0.0);

        state.process_frame(&mut frame_buf, &input_buf);

        // RNNoise has one frame of algorithmic latency: wet sample `w` lines up
        // with dry sample `w - frame_size`. Frame 0 therefore maps entirely
        // before the start of the signal and is dropped -- the same result the
        // previous `wet[i + frame_size]` lookup produced, which also left the
        // final `frame_size` dry samples with no wet partner.
        if f == 0 {
            continue;
        }
        let dst_start = (f - 1) * frame_size;
        let n = frame_size.min(dry_len - dst_start);
        for k in 0..n {
            out[dst_start + k] += frame_buf[k] / 32768.0 * mix;
        }
    }

    Ok(out)
}

// ===========================================================================
// MODULE 4: Dereverb via DeepFilterNet3 CLI (subprocess)
// ===========================================================================
//
// Requires `deep-filter` binary (https://github.com/Rikorose/DeepFilterNet/releases)
// in the `models` folder next to VOCAN (see `dfn3_binary_path`). Expects 48 kHz mono f32 signal. Returns the same.
//
// DeepFilterNet3 performs joint denoise + dereverb at 48 kHz — no resampling required.
// The -D flag compensates for STFT latency and model lookahead (sample-accurate),
// so wet and dry are synchronized without additional shifting.
//
// WAV I/O is handled natively via the `hound` crate, eliminating two FFmpeg
// subprocess calls and two temporary files compared to the previous approach.

pub struct DereverbParams {
    /// Dry/wet mix. 1.0 = full effect, 0.7–0.85 usually sounds more natural.
    pub mix: f32,
    /// 0..=100, corresponds to --atten-lim-db in CLI. 100 = full effect,
    /// 20–30 = subtle (preserves some reverb for naturalness).
    pub attenuation_limit: f32,
    /// Post-filter (--pf) — more aggressively suppresses highly noisy parts.
    /// Leave false for clean dubbing; enable for outdoor/field recordings.
    pub post_filter: bool,
}

impl Default for DereverbParams {
    fn default() -> Self {
        Self {
            mix: 0.8,
            attenuation_limit: 30.0,
            post_filter: false,
        }
    }
}

/// Writes f32 mono samples at 48 kHz to a WAV file using `hound`.
fn write_wav_f32(path: &Path, samples: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        sample_rate: 48000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
        channels: 1,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("cannot create WAV file: {}", path.display()))?;
    for &s in samples {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Reads a WAV file and returns f32 mono samples using `hound`.
fn read_wav_f32(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("cannot open WAV file: {}", path.display()))?;
    let spec = reader.spec();

    // Convert any sample format to f32.
    //
    // Decode errors are propagated rather than filtered away. The previous
    // `.filter_map(|s| s.ok())` turned a truncated or malformed WAV into a
    // silently *shorter* signal, and the dry/wet mix below then padded the
    // missing tail with dry audio -- producing a file that looked correct but
    // had quietly lost its dereverb from some arbitrary point onwards.
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<f32>, _>>()
            .with_context(|| format!("corrupt WAV sample data in {}", path.display()))?,
        hound::SampleFormat::Int => {
            if spec.bits_per_sample == 0 || spec.bits_per_sample > 32 {
                anyhow::bail!(
                    "unsupported bit depth {} in {}",
                    spec.bits_per_sample,
                    path.display()
                );
            }
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max_val))
                .collect::<Result<Vec<f32>, _>>()
                .with_context(|| format!("corrupt WAV sample data in {}", path.display()))?
        }
    };

    // Downmix to mono if stereo (average channels).
    let mono: Vec<f32> = if spec.channels == 1 {
        samples
    } else {
        let ch = spec.channels as usize;
        let frames = samples.len() / ch;
        (0..frames)
            .map(|i| {
                let start = i * ch;
                samples[start..start + ch].iter().sum::<f32>() / ch as f32
            })
            .collect()
    };

    Ok(mono)
}

/// File name of the DeepFilterNet3 command-line binary on this platform.
pub const DFN3_BINARY: &str = if cfg!(windows) {
    "deep-filter.exe"
} else {
    "deep-filter"
};

/// Environment variable naming the `deep-filter` binary explicitly. The
/// installers set it for the test run, which executes from `target/.../deps`
/// rather than from the app folder.
pub const DFN3_ENV: &str = "VOCAN_DEEPFILTER";

/// Where the DeepFilterNet3 binary is, if it is anywhere VOCAN looks.
///
/// Order: the file named by [`DFN3_ENV`] (a path that does not exist is
/// skipped), then `models/deep-filter` next to our executable (where the
/// installers put it), then next to ffmpeg when ffmpeg was found by an
/// explicit path (older or manual installs). When none exists, returns the
/// `models/` location, so error messages say where to put the file.
///
/// The processing pipeline and the status indicator in the UI both go through
/// here, so what the indicator reports is exactly what a run will try to open.
pub fn dfn3_binary_path(ffmpeg: &Path) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    dfn3_binary_path_in(std::env::var_os(DFN3_ENV), exe_dir.as_deref(), ffmpeg)
}

fn dfn3_binary_path_in(
    env: Option<std::ffi::OsString>,
    exe_dir: Option<&Path>,
    ffmpeg: &Path,
) -> Option<PathBuf> {
    let from_env = env
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    if from_env.is_some() {
        return from_env;
    }
    let in_models = exe_dir.map(|d| d.join("models").join(DFN3_BINARY));
    let next_to_ffmpeg = ffmpeg
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(DFN3_BINARY));
    [in_models.clone(), next_to_ffmpeg]
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
        .or(in_models)
}

/// Whether the DeepFilterNet3 binary is present where [`dfn3_binary_path`]
/// looks for it.
pub fn dfn3_available(ffmpeg: &Path) -> bool {
    dfn3_binary_path(ffmpeg).is_some_and(|p| p.is_file())
}

pub fn apply_dereverb_dfn3(
    samples_48k_mono: &[f32],
    params: &DereverbParams,
    dfn_binary: &Path,
    _ffmpeg: &Path,
) -> Result<Vec<f32>> {
    if !dfn_binary.exists() {
        anyhow::bail!("deep-filter binary not found at: {}", dfn_binary.display());
    }

    // 1. Write input WAV using hound (no FFmpeg subprocess needed).
    let scratch = crate::proc::scratch_dir();
    let mut temp_builder = tempfile::Builder::new();
    temp_builder.suffix(".wav");
    let temp_wav_in = match scratch {
        Some(dir) => temp_builder.tempfile_in(dir),
        None => temp_builder.tempfile(),
    }
    .context("cannot create temp wav input")?;
    let wav_in_path = temp_wav_in.path().to_path_buf();
    write_wav_f32(&wav_in_path, samples_48k_mono)?;

    // 2. DeepFilterNet3 inference.
    // We use a dedicated temp directory and take the first produced WAV
    // instead of guessing the naming scheme (which differs between CLI builds).
    let out_dir = match scratch {
        Some(dir) => tempfile::tempdir_in(dir),
        None => tempfile::tempdir(),
    }
    .context("cannot create temp dir for deep-filter")?;

    let mut cmd = silent_command(dfn_binary);
    cmd.arg("-D")
        .args([
            "-a",
            &format!("{:.1}", params.attenuation_limit.clamp(0.0, 100.0)),
        ])
        .arg("-o")
        .arg(out_dir.path())
        .arg(&wav_in_path);
    if params.post_filter {
        cmd.arg("--pf");
    }

    // Supervised: DeepFilterNet3 is the slowest stage in the pipeline and the
    // most likely to wedge, so it has to be interruptible from the UI thread.
    let out = crate::proc::output_supervised(&mut cmd).context("deep-filter invocation failed")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("deep-filter returned non-zero: {}", stderr);
    }

    // 3. Find output file (there should be exactly one WAV in out_dir).
    let wav_out_path = std::fs::read_dir(out_dir.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|x| x == "wav").unwrap_or(false))
        .context("deep-filter produced no WAV output")?;

    // 4. Read output WAV using hound (no FFmpeg subprocess needed).
    let wet = read_wav_f32(&wav_out_path)?;

    // 5. Dry/wet mix. With -D, wet is already sample-accurate synchronized.
    let mix = params.mix.clamp(0.0, 1.0);
    let n = samples_48k_mono.len().min(wet.len());
    let mut out_samples = Vec::with_capacity(samples_48k_mono.len());
    for i in 0..n {
        out_samples.push(samples_48k_mono[i] * (1.0 - mix) + wet[i] * mix);
    }
    // If wet was shorter (it shouldn't be), fill with dry.
    out_samples.extend(samples_48k_mono.iter().skip(n).copied());

    Ok(out_samples)
}

// ===========================================================================
// MODULE 5: Smart Downward Expander (noise-floor-based, bounded)
// ===========================================================================
//
// Detects a noise floor via windowed RMS analysis, then applies a gentle,
// bounded downward expansion to content sitting below that floor.
// This is NOT a hard gate — attenuation is capped at the selected reduction
// profile's dB value, and a soft knee + hold time prevent chattering.
//
// Pipeline placement: after dereverb (DFN3), before spectral gate / nnnoise.
// Noise floor estimation runs on the pre-dereverb signal (the earliest
// in-memory samples available in Rust).

/// Minimum margin in dB (maps to 0% on the Safety Margin slider).
const MARGIN_DB_MIN: f32 = 2.0;
/// Maximum margin in dB (maps to 100% on the Safety Margin slider).
const MARGIN_DB_MAX: f32 = 8.0;

/// Converts the UI-facing safety percentage (0–100) to an internal margin in dB.
///
/// Higher percentage = more conservative = larger margin = less aggressive.
/// This is the core safety mechanism: the threshold sits `margin_db` below
/// the detected noise floor, so breaths, room tone, and quiet vocal nuance
/// are never touched.
pub fn safety_pct_to_margin_db(safety_pct: f32) -> f32 {
    let pct = safety_pct.clamp(0.0, 100.0);
    MARGIN_DB_MIN + (pct / 100.0) * (MARGIN_DB_MAX - MARGIN_DB_MIN)
}

/// Parameters for the downward expander.
///
/// `safety_pct` is the UI-facing source of truth (0–100%). `margin_db` is
/// derived from it at call time via `safety_pct_to_margin_db`.
/// `reduction_profile` determines the maximum attenuation (bounded, not -inf).
/// The remaining fields are advanced/hidden — sane defaults are provided.
pub struct ExpanderParams {
    /// 0–100, default 50.0. UI-facing "Safety Margin".
    pub safety_pct: f32,
    /// Preset reduction depth. Default: Recommended (-12.0 dB).
    pub reduction_profile: ReductionProfile,
    /// Soft knee width in dB. Default: 3.0.
    pub knee_db: f32,
    /// Attack time in ms. Default: 10.0.
    pub attack_ms: f32,
    /// Release time in ms. Default: 200.0 (slow to avoid pumping).
    pub release_ms: f32,
    /// Hold time in ms. Default: 60.0 (prevents chattering on quiet consonants/tails).
    pub hold_ms: f32,
}

impl Default for ExpanderParams {
    fn default() -> Self {
        Self {
            safety_pct: 50.0,
            reduction_profile: ReductionProfile::Recommended,
            knee_db: 3.0,
            attack_ms: 10.0,
            release_ms: 200.0,
            hold_ms: 60.0,
        }
    }
}

/// Estimates the noise floor of a signal in dB using windowed RMS.
///
/// Uses 25 ms windows with 50% overlap. Builds a sorted distribution of
/// window RMS values (in dB) and takes the p12 percentile — not the raw
/// minimum, which would pick up digital silence artifacts (-inf dB) and
/// single outliers.
///
/// Returns `None` if the file is too short or has too little low-level
/// content to build a reliable distribution (e.g. single-word dubbing lines).
/// The caller should bypass the expander stage in that case.
///
/// **Must run on the raw/pre-dereverb signal** — denoising distorts the
/// floor characteristic and will bias the estimate.
pub fn estimate_noise_floor_db(samples: &[f32], sample_rate: u32) -> Option<f32> {
    if samples.is_empty() || sample_rate == 0 {
        return None;
    }

    let win_len = (sample_rate as f32 * 0.025).round() as usize; // 25 ms
    let hop = win_len / 2; // 50% overlap

    // Three separate degeneracies collapse into one bound. All need an absurd
    // sample rate (below ~100 Hz), but this function is public, so none of them
    // is unreachable:
    //   * win_len == 1 -> hop == 0, and the analysis loop below advances by
    //     `hop`, so it spins forever. A hang, which `catch_unwind` cannot
    //     recover and the Stop button cannot interrupt.
    //   * win_len == 1 -> the Hann window divides by `win_len - 1`, making
    //     every coefficient 0.0/0.0 = NaN.
    //   * win_len == 2 -> the window is [0.0, 0.0] (a Hann window is zero at
    //     both endpoints, and length 2 is nothing but endpoints), so
    //     `w2_sum` is 0 and `inv_w2_sum` is infinite -- every RMS comes out NaN.
    // Only win_len >= 3 gives a window with real energy in it. The NaN cases
    // are not unsafe on their own (NaN fails the `rms > 1e-7` test, so the
    // function returns None anyway), but reaching that answer costs a full
    // O(n) pass over the signal.
    if win_len < 3 || samples.len() < win_len {
        return None;
    }

    // Hann window for RMS calculation.
    let window: Vec<f32> = (0..win_len)
        .map(|n| {
            let x = n as f32 / (win_len - 1) as f32;
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
        })
        .collect();

    // Normalizing the windowed energy by the window's own sum of squares --
    // rather than by the sample count -- is what makes the result an actual
    // RMS. Dividing by `win_len` instead understates every window by
    // 20*log10(sqrt(3/8)) = -4.26 dB for a Hann window, a constant bias that
    // pushed the detected floor (and so the expander threshold derived from it)
    // that much lower than the documented safety margin implies.
    let w2_sum: f32 = window.iter().map(|w| w * w).sum();
    let inv_w2_sum = 1.0 / w2_sum;

    // Collect RMS values (in dB) for all windows.
    let mut rms_db_values: Vec<f32> = Vec::with_capacity(samples.len() / hop + 1);
    let mut pos = 0usize;

    while pos + win_len <= samples.len() {
        let mut sum_sq = 0.0f32;
        for n in 0..win_len {
            let s = samples[pos + n] * window[n];
            sum_sq += s * s;
        }
        let rms = (sum_sq * inv_w2_sum).sqrt();

        // Skip digital silence / near-silence (-inf or extreme outliers).
        if rms > 1e-7 {
            let db = 20.0 * rms.log10();
            rms_db_values.push(db);
        }

        pos += hop;
    }

    // Need at least 10 valid windows for a reliable percentile.
    if rms_db_values.len() < 10 {
        return None;
    }

    // Take p12 (12th percentile). Only that one order statistic is needed, so
    // partition around it in O(n) instead of fully sorting the distribution.
    let idx = (rms_db_values.len() as f32 * 0.12).round() as usize;
    let idx = idx.min(rms_db_values.len() - 1);
    rms_db_values.select_nth_unstable_by(idx, |a, b| {
        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
    });

    Some(rms_db_values[idx])
}

/// Applies a bounded downward expander to the signal.
///
/// The expander reduces gain for content below `threshold_db` (derived from
/// the noise floor minus the safety margin), with a soft knee transition.
/// Attenuation is capped at `reduction_profile.db()` — this is bounded
/// expansion, not a hard gate to -inf.
///
/// Uses RMS detection (not peak) for the level detector, with attack/release
/// smoothing and a mandatory hold time to prevent chattering on soft
/// word-endings and quiet consonants.
///
/// Processes `buf` in place — infallible (no I/O, no external calls).
///
/// In-place is safe here because each sample is read and written at the same
/// index and never read again afterwards; this mirrors
/// [`apply_adaptive_eq_inplace`] and avoids a full-length copy of the signal for
/// what is otherwise a strictly element-wise gain.
pub fn apply_expander_inplace(
    buf: &mut [f32],
    sample_rate: u32,
    channels: u16,
    params: &ExpanderParams,
    noise_floor_db: f32,
) {
    if buf.is_empty() || channels == 0 || sample_rate == 0 {
        return;
    }

    let ch = channels as usize;
    let frames = buf.len() / ch;

    // Derive parameters.
    let margin_db = safety_pct_to_margin_db(params.safety_pct);
    let threshold_db = noise_floor_db - margin_db;
    let max_reduction_db = params.reduction_profile.db();
    let knee_db = params.knee_db.max(0.1);
    let min_gain_lin = 10.0f32.powf(max_reduction_db / 20.0); // capped, never 0

    // RMS detector window: ~3 ms (short enough to track, long enough to be stable).
    let rms_win = (sample_rate as f32 * 0.003).round() as usize;
    let rms_win = rms_win.max(1);

    // Envelope coefficients (one-pole, per-sample).
    let attack_coef = (-1.0 / (params.attack_ms * 0.001 * sample_rate as f32)).exp();
    let release_coef = (-1.0 / (params.release_ms * 0.001 * sample_rate as f32)).exp();

    // Hold time in samples.
    let hold_samples = (params.hold_ms * 0.001 * sample_rate as f32).round() as usize;

    // 10^(x/20) == 2^(x * log2(10)/20). `exp2` is materially cheaper than the
    // general `powf`, which internally does this same rewrite plus edge-case
    // handling that a finite dB value never needs.
    let db_to_log2 = std::f32::consts::LOG2_10 / 20.0;
    let inv_rms_win = 1.0 / rms_win as f32;

    for c in 0..ch {
        // Per-channel state.
        let mut rms_sum_sq = 0.0f32; // running sum of squares for RMS
        let mut rms_buf = vec![0.0f32; rms_win]; // circular buffer
        let mut rms_buf_idx = 0usize;
        // Envelope in dB. Bootstrapped from the first frame's det_db (below)
        // rather than started at f32::MIN: with the one-pole attack/release
        // smoothing below, climbing from f32::MIN to a normal dB range takes
        // ~40000 samples (~800ms at 48kHz) regardless of actual signal
        // level, producing a spurious "attack from silence" at the start of
        // every processed file.
        let mut env_db = 0.0f32;
        let mut env_initialized = false;
        let mut gain_lin = 1.0f32; // current smoothed gain
        let mut hold_counter = 0usize;

        for i in 0..frames {
            let sample = buf[i * ch + c];

            // --- RMS detector (circular buffer) ---
            let old_sq = rms_buf[rms_buf_idx];
            let new_sq = sample * sample;
            rms_buf[rms_buf_idx] = new_sq;
            rms_buf_idx += 1;
            if rms_buf_idx == rms_win {
                rms_buf_idx = 0;
            }
            // The drift guard writes back into the accumulator: clamping only a
            // local copy would let a negative running sum persist and re-poison
            // every later iteration.
            rms_sum_sq = (rms_sum_sq - old_sq + new_sq).max(0.0);
            let mean_sq = rms_sum_sq * inv_rms_win;
            // 20*log10(sqrt(m)) == 10*log10(m), so the per-sample sqrt is
            // redundant. The threshold matches the old `rms > 1e-10`.
            let det_db = if mean_sq > 1e-20 {
                10.0 * mean_sq.log10()
            } else {
                -200.0 // effectively -inf
            };

            // --- Level envelope (attack/release on dB) ---
            if !env_initialized {
                env_db = det_db;
                env_initialized = true;
            } else {
                let coef = if det_db > env_db {
                    attack_coef
                } else {
                    release_coef
                };
                env_db = det_db + coef * (env_db - det_db);
            }

            // --- Gain computer (soft knee, bounded) ---
            //
            //   level_db >= threshold_db                        → gain = 1.0
            //   threshold_db - knee_db < level_db < threshold_db → linear interp
            //   level_db <= threshold_db - knee_db              → gain = max_reduction
            //
            // The knee region smoothly transitions from 0 dB to max_reduction_db.
            let target_gain = if env_db >= threshold_db {
                1.0
            } else if env_db > threshold_db - knee_db {
                // In the knee: linear interpolation in dB.
                // At threshold_db → 0 dB reduction (gain 1.0)
                // At threshold_db - knee_db → max_reduction_db (gain = min_gain_lin)
                let ratio = (threshold_db - env_db) / knee_db; // 0..1
                let gain_db = max_reduction_db * ratio;
                (gain_db * db_to_log2).exp2()
            } else {
                min_gain_lin
            };

            // --- Hold logic ---
            //
            // When the signal drops below threshold, start counting a hold timer.
            // While the timer is active, freeze the gain (don't decrease further).
            // This prevents the expander from flapping open/close on soft
            // word-endings and quiet consonants.
            if env_db < threshold_db {
                if hold_counter < hold_samples {
                    hold_counter += 1;
                    // During hold, don't let gain decrease — keep it at least
                    // as high as the current smoothed gain.
                    // (target_gain may be lower, but we freeze.)
                }
                // After hold expires, target_gain applies normally.
            } else {
                // Signal rose above threshold — reset hold.
                hold_counter = 0;
            }

            // If in hold period, prevent gain from decreasing. Strictly less
            // than `hold_samples`: once the counter reaches `hold_samples`
            // (hold has fully elapsed), target_gain must apply normally again.
            // (A `<=` here would freeze the gain permanently on any signal
            // that stays quiet longer than hold_ms, since the counter above
            // is clamped at `hold_samples` and never grows past it.)
            let effective_target = if hold_counter > 0 && hold_counter < hold_samples {
                gain_lin.max(target_gain)
            } else {
                target_gain
            };

            // --- Gain smoothing (attack/release on gain itself) ---
            let gain_coef = if effective_target > gain_lin {
                attack_coef // opening (gain rising) — attack
            } else {
                release_coef // closing (gain falling) — release
            };
            gain_lin = effective_target + gain_coef * (gain_lin - effective_target);

            // Apply gain.
            buf[i * ch + c] = sample * gain_lin;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dfn3_path_defaults_to_models_next_to_our_executable() {
        // Nothing exists anywhere: the answer is where the file should go. A
        // bare `ffmpeg` has an empty parent and must not turn into a lookup in
        // the current directory.
        let exe = tempfile::tempdir().unwrap();
        assert_eq!(
            dfn3_binary_path_in(None, Some(exe.path()), Path::new("ffmpeg")),
            Some(exe.path().join("models").join(DFN3_BINARY))
        );
    }

    #[test]
    fn dfn3_models_folder_wins_over_the_ffmpeg_folder() {
        let exe = tempfile::tempdir().unwrap();
        let ff = tempfile::tempdir().unwrap();
        let ffmpeg = ff.path().join("ffmpeg");
        std::fs::write(ff.path().join(DFN3_BINARY), b"x").unwrap();
        // Only the ffmpeg-side copy exists: it is found.
        assert_eq!(
            dfn3_binary_path_in(None, Some(exe.path()), &ffmpeg),
            Some(ff.path().join(DFN3_BINARY))
        );
        // Once models/ has one too, that one wins.
        std::fs::create_dir(exe.path().join("models")).unwrap();
        let in_models = exe.path().join("models").join(DFN3_BINARY);
        std::fs::write(&in_models, b"x").unwrap();
        assert_eq!(
            dfn3_binary_path_in(None, Some(exe.path()), &ffmpeg),
            Some(in_models)
        );
    }

    #[test]
    fn dfn3_env_wins_but_a_missing_file_is_skipped() {
        let exe = tempfile::tempdir().unwrap();
        std::fs::create_dir(exe.path().join("models")).unwrap();
        let in_models = exe.path().join("models").join(DFN3_BINARY);
        std::fs::write(&in_models, b"x").unwrap();
        let custom = exe.path().join("custom-df");
        std::fs::write(&custom, b"x").unwrap();
        let ffmpeg = Path::new("ffmpeg");
        assert_eq!(
            dfn3_binary_path_in(Some(custom.clone().into()), Some(exe.path()), ffmpeg),
            Some(custom)
        );
        let missing = exe.path().join("nope");
        assert_eq!(
            dfn3_binary_path_in(Some(missing.into()), Some(exe.path()), ffmpeg),
            Some(in_models)
        );
    }

    #[test]
    fn dfn3_available_follows_the_file() {
        // `dfn3_available` reads the real environment, so exercise the same
        // decision through `dfn3_binary_path_in` on a scratch folder.
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        let ffmpeg = Path::new("ffmpeg");
        let found =
            |d: &Path| dfn3_binary_path_in(None, Some(d), ffmpeg).is_some_and(|p| p.is_file());
        assert!(!found(dir.path()));
        // A directory with the right name is not the binary.
        std::fs::create_dir(models.join(DFN3_BINARY)).unwrap();
        assert!(!found(dir.path()));
        std::fs::remove_dir(models.join(DFN3_BINARY)).unwrap();
        std::fs::write(models.join(DFN3_BINARY), b"x").unwrap();
        assert!(found(dir.path()));
    }

    // -----------------------------------------------------------------------
    // Synthetic signal helpers (no external `rand` dependency).
    // -----------------------------------------------------------------------

    fn sine_wave(freq_hz: f32, sample_rate: u32, secs: f32, amplitude: f32) -> Vec<f32> {
        let n = (sample_rate as f32 * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin()
            })
            .collect()
    }

    /// Deterministic xorshift32 white noise generator, values in `[-amplitude, amplitude]`.
    fn xorshift_noise(len: usize, amplitude: f32, seed: u32) -> Vec<f32> {
        let mut state = seed.max(1);
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let normalized = (state as f32 / u32::MAX as f32) * 2.0 - 1.0;
                normalized * amplitude
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    // -----------------------------------------------------------------------
    // safety_pct_to_margin_db -- formula documented in README:
    // margin_db = 2.0 + (pct/100) * (8.0 - 2.0)
    // -----------------------------------------------------------------------

    #[test]
    fn safety_pct_to_margin_db_matches_formula_at_endpoints_and_midpoint() {
        assert!((safety_pct_to_margin_db(0.0) - 2.0).abs() < 1e-6);
        assert!((safety_pct_to_margin_db(50.0) - 5.0).abs() < 1e-6);
        assert!((safety_pct_to_margin_db(100.0) - 8.0).abs() < 1e-6);
    }

    #[test]
    fn safety_pct_to_margin_db_clamps_out_of_range_input() {
        assert!((safety_pct_to_margin_db(-10.0) - 2.0).abs() < 1e-6);
        assert!((safety_pct_to_margin_db(150.0) - 8.0).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // estimate_noise_floor_db
    // -----------------------------------------------------------------------

    #[test]
    fn estimate_noise_floor_db_on_empty_returns_none() {
        assert_eq!(estimate_noise_floor_db(&[], 48000), None);
    }

    #[test]
    fn estimate_noise_floor_db_on_zero_sample_rate_returns_none() {
        let samples = xorshift_noise(48000, 0.1, 1);
        assert_eq!(estimate_noise_floor_db(&samples, 0), None);
    }

    #[test]
    fn estimate_noise_floor_db_too_short_returns_none() {
        // win_len at 48kHz is round(48000*0.025) = 1200 samples; 500 < that.
        let samples = xorshift_noise(500, 0.1, 1);
        assert_eq!(estimate_noise_floor_db(&samples, 48000), None);
    }

    #[test]
    fn estimate_noise_floor_db_at_absurdly_low_sample_rate_returns_none_without_hanging() {
        // Regression test for a hang, so it has to terminate at all to pass.
        //
        // sample_rate 40 -> win_len = round(40 * 0.025) = 1 -> hop = 0. The
        // analysis loop advances by `hop`, so it used to spin forever on any
        // input long enough to enter it: a hang that catch_unwind cannot
        // recover and Stop cannot interrupt.
        let noise = xorshift_noise(4000, 0.2, 9);
        assert_eq!(estimate_noise_floor_db(&noise, 40), None);
        // sample_rate 60 -> win_len = 2. The loop terminates, but a Hann window
        // of length 2 is [0.0, 0.0], so w2_sum is 0 and every RMS is NaN.
        assert_eq!(estimate_noise_floor_db(&noise, 60), None);
    }

    #[test]
    fn estimate_noise_floor_db_accepts_the_smallest_non_degenerate_window() {
        // sample_rate 100 -> win_len = round(2.5) = 3, the shortest Hann window
        // with any energy in it. The guard above must not swallow this case.
        let noise = xorshift_noise(4000, 0.2, 9);
        assert!(estimate_noise_floor_db(&noise, 100).is_some());
    }

    #[test]
    fn estimate_noise_floor_db_on_pure_silence_returns_none() {
        let samples = vec![0.0f32; 96000]; // 2s at 48kHz, well above the length floor
        assert_eq!(estimate_noise_floor_db(&samples, 48000), None);
    }

    #[test]
    fn estimate_noise_floor_db_is_absolutely_calibrated_against_a_known_sine() {
        // A steady sine of amplitude A has RMS A/sqrt(2), so a correctly
        // normalized windowed RMS must report 20*log10(A/sqrt(2)) dB.
        //
        // This is the test the old implementation failed: normalizing the
        // Hann-windowed energy by `win_len` instead of by the window's own sum
        // of squares understated every window by 20*log10(sqrt(3/8)) = 4.26 dB.
        // The relative test below could not catch that, because the bias is
        // multiplicative and cancels between two amplitudes.
        let amplitude = 0.25f32;
        let samples = sine_wave(440.0, 48000, 2.0, amplitude);
        let measured = estimate_noise_floor_db(&samples, 48000).expect("floor");
        let expected = 20.0 * (amplitude / 2.0f32.sqrt()).log10();
        assert!(
            (measured - expected).abs() < 0.5,
            "measured {measured} dB, expected ~{expected} dB"
        );
    }

    #[test]
    fn estimate_noise_floor_db_scales_exactly_with_amplitude() {
        // Same underlying noise, two amplitudes -- windowing/percentile selection
        // is purely multiplicative, so the estimated floor must shift by exactly
        // 20*log10(ratio), regardless of window-shape implementation details.
        let base = xorshift_noise(96000, 1.0, 42);
        let loud: Vec<f32> = base.iter().map(|s| s * 0.5).collect();
        let quiet: Vec<f32> = base.iter().map(|s| s * 0.05).collect();

        let floor_loud = estimate_noise_floor_db(&loud, 48000).expect("loud floor");
        let floor_quiet = estimate_noise_floor_db(&quiet, 48000).expect("quiet floor");

        let expected_diff_db = 20.0 * (0.5f32 / 0.05f32).log10(); // 20 dB
        assert!(
            (floor_loud - floor_quiet - expected_diff_db).abs() < 0.1,
            "floor_loud={floor_loud}, floor_quiet={floor_quiet}, expected diff={expected_diff_db}"
        );
    }

    // -----------------------------------------------------------------------
    // apply_expander
    // -----------------------------------------------------------------------

    #[test]
    fn apply_expander_on_empty_signal_is_a_no_op() {
        let params = ExpanderParams::default();
        let mut buf: Vec<f32> = Vec::new();
        apply_expander_inplace(&mut buf, 48000, 1, &params, -40.0);
        assert!(buf.is_empty());
    }

    #[test]
    fn apply_expander_zero_channels_leaves_input_unchanged() {
        let samples = sine_wave(440.0, 48000, 0.1, 0.5);
        let params = ExpanderParams::default();
        let mut buf = samples.clone();
        apply_expander_inplace(&mut buf, 48000, 0, &params, -40.0);
        assert_eq!(buf, samples);
    }

    #[test]
    fn apply_expander_never_produces_nan_or_inf() {
        let mut samples = xorshift_noise(48000, 0.3, 7);
        samples.extend(vec![0.0f32; 4800]); // silence segment
        samples.extend(sine_wave(880.0, 48000, 0.5, 0.8));
        let params = ExpanderParams::default();
        apply_expander_inplace(&mut samples, 48000, 1, &params, -40.0);
        assert!(samples.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn apply_expander_loud_signal_stays_near_unity_gain_after_attack() {
        // A sine well above the noise floor for its whole duration; after the
        // ~10ms default attack settles, gain should be close to 1.0.
        let sr = 48000u32;
        let samples = sine_wave(440.0, sr, 1.0, 0.5);
        let params = ExpanderParams::default();
        let mut out = samples.clone();
        apply_expander_inplace(&mut out, sr, 1, &params, -40.0);

        // Skip the first 50ms (attack warm-up) and compare the tail.
        let warmup = (sr as f32 * 0.05) as usize;
        let in_tail_rms = rms(&samples[warmup..]);
        let out_tail_rms = rms(&out[warmup..]);
        let ratio = out_tail_rms / in_tail_rms;
        assert!(
            (0.9..=1.1).contains(&ratio),
            "expected near-unity gain, got ratio={ratio}"
        );
    }

    #[test]
    fn apply_expander_deeply_quiet_signal_settles_near_reduction_profile_cap() {
        // A very quiet, sustained tone far below (noise_floor - margin), held
        // long enough (2s >> attack/release/hold time constants) for the
        // envelope to fully settle to the profile's capped attenuation.
        let sr = 48000u32;
        let samples = sine_wave(440.0, sr, 2.0, 0.001); // very low level
        let params = ExpanderParams {
            reduction_profile: ReductionProfile::Recommended, // -12.0 dB cap
            ..Default::default()
        };
        // noise_floor_db well above the signal level so it's deep in the gate region.
        let mut out = samples.clone();
        apply_expander_inplace(&mut out, sr, 1, &params, -20.0);

        let tail_start = samples.len() - 4800; // last 100ms
        let in_tail_rms = rms(&samples[tail_start..]);
        let out_tail_rms = rms(&out[tail_start..]);
        let measured_gain_db = 20.0 * (out_tail_rms / in_tail_rms).log10();

        assert!(
            (measured_gain_db - (-12.0)).abs() < 1.0,
            "expected settled gain near -12.0 dB, got {measured_gain_db}"
        );
    }

    // -----------------------------------------------------------------------
    // apply_spectral_gate
    //
    // The gate compares FFT-bin magnitudes (unnormalized) against a
    // dB-derived linear threshold, so its absolute numeric behavior depends
    // on FFT-size/window normalization details. Rather than assert exact
    // gain values (which would require running the real FFT to verify),
    // these tests stick to invariants that hold regardless of that scale:
    // output length, finiteness, and monotonicity in input level.
    // -----------------------------------------------------------------------

    #[test]
    fn apply_spectral_gate_output_length_matches_input() {
        let samples = xorshift_noise(5000, 0.2, 3);
        let out = apply_spectral_gate(&samples, 48000, 1, &SpectralGateParams::default()).unwrap();
        assert_eq!(out.len(), samples.len());
    }

    #[test]
    fn apply_spectral_gate_shorter_than_fft_window_is_still_processed() {
        // Previously the STFT loop only ran where a whole 2048-sample analysis
        // window fit inside the signal, so anything shorter than fft_size came
        // back as pure silence. The padded STFT covers short inputs properly.
        let samples = xorshift_noise(100, 0.2, 3); // shorter than fft_size (2048)
        let out = apply_spectral_gate(&samples, 48000, 1, &SpectralGateParams::default()).unwrap();
        assert_eq!(out.len(), samples.len());
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(
            out.iter().any(|s| *s != 0.0),
            "short input must not be silenced outright"
        );
    }

    #[test]
    fn apply_spectral_gate_preserves_the_tail_of_the_signal() {
        // Regression test: the STFT loop used to stop at the last position
        // where a full window fit, leaving up to fft_size - 1 samples (~43ms at
        // 48kHz) never written and therefore silent at the end of every file.
        let sr = 48000u32;
        let samples = sine_wave(440.0, sr, 1.0, 0.5);
        let out = apply_spectral_gate(&samples, sr, 1, &SpectralGateParams::default()).unwrap();
        assert_eq!(out.len(), samples.len());

        // The last 2048 samples are exactly the region the old loop dropped.
        let tail = &out[out.len() - 2048..];
        let mid = &out[out.len() / 2 - 1024..out.len() / 2 + 1024];
        let tail_rms = rms(tail);
        let mid_rms = rms(mid);
        assert!(
            tail_rms > 0.5 * mid_rms,
            "tail was attenuated relative to the middle: tail_rms={tail_rms}, mid_rms={mid_rms}"
        );
    }

    #[test]
    fn apply_spectral_gate_passes_a_loud_tone_through_at_roughly_unity() {
        // The Hann^2 / 75%-overlap COLA sum is 1.5, which the 2/3 compensation
        // factor inverts -- so a tone well above the threshold should come back
        // at close to its original level. This pins the padding fix and the
        // compensation factor together; before the fix the edges alone made
        // whole-signal RMS come out low.
        let sr = 48000u32;
        let samples = sine_wave(440.0, sr, 0.5, 0.5);
        let out = apply_spectral_gate(&samples, sr, 1, &SpectralGateParams::default()).unwrap();
        let ratio = rms(&out) / rms(&samples);
        assert!(
            (0.9..=1.1).contains(&ratio),
            "expected near-unity throughput, got ratio={ratio}"
        );
    }

    #[test]
    fn apply_spectral_gate_zero_channels_leaves_input_unchanged() {
        // `frames_total = samples.len() / ch` panicked at ch == 0. Mirrors
        // apply_expander_zero_channels_leaves_input_unchanged.
        let samples = sine_wave(440.0, 48000, 0.1, 0.5);
        let out = apply_spectral_gate(&samples, 48000, 0, &SpectralGateParams::default()).unwrap();
        assert_eq!(out, samples);
    }

    #[test]
    fn apply_spectral_gate_never_produces_nan_or_inf() {
        let samples = xorshift_noise(96000, 0.3, 5);
        let out = apply_spectral_gate(&samples, 48000, 1, &SpectralGateParams::default()).unwrap();
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn apply_spectral_gate_is_monotonic_in_input_level() {
        // The gate's gain is a non-decreasing function of the per-bin envelope,
        // which itself scales with input amplitude -- so a louder copy of the
        // same waveform must never produce a quieter output.
        let quiet = xorshift_noise(96000, 0.05, 5);
        let loud: Vec<f32> = quiet.iter().map(|s| s * 10.0).collect();

        let out_quiet =
            apply_spectral_gate(&quiet, 48000, 1, &SpectralGateParams::default()).unwrap();
        let out_loud =
            apply_spectral_gate(&loud, 48000, 1, &SpectralGateParams::default()).unwrap();

        assert!(rms(&out_loud) >= rms(&out_quiet));
    }

    // -----------------------------------------------------------------------
    // Adaptive EQ: the decision, on made-up measurements
    // -----------------------------------------------------------------------

    const SR: u32 = 48_000;

    /// A measurement that matches the reference exactly, offset by `level_db`
    /// (dBFS and the table share no reference), with plenty of speech and a
    /// background far below it.
    fn on_target(level_db: f32) -> VoiceBands {
        let target = speech_target_db(SR as f32 / 2.0);
        VoiceBands {
            speech_db: target.map(|t| t.map(|t| t + level_db)),
            snr_db: target.map(|t| t.map(|_| 40.0)),
            speech_secs: 10.0,
        }
    }

    /// `on_target(-30.0)` with `deviation_db` added to the measured bands: a
    /// positive deviation means the band is hotter than the reference.
    fn deviated(deviation_db: [f32; EQ_BANDS]) -> VoiceBands {
        let mut bands = on_target(-30.0);
        for (band, deviation) in bands.speech_db.iter_mut().zip(deviation_db) {
            *band = band.map(|m| m + deviation);
        }
        bands
    }

    #[test]
    fn a_take_on_the_reference_is_left_alone_at_any_level() {
        for level in [0.0, -30.0, -60.0] {
            let plan = plan_adaptive_eq(&on_target(level), SR);
            assert_eq!(plan.bypass, None);
            assert!(
                plan.gains_db.iter().all(|g| g.abs() < 0.01),
                "level {level}: {:?}",
                plan.gains_db
            );
        }
    }

    #[test]
    fn a_small_deviation_is_corrected_by_half_in_the_opposite_direction() {
        let plan = plan_adaptive_eq(&deviated([0.0, 1.0, -1.0, 0.0]), SR);
        assert!((plan.gains_db[0]).abs() < 0.01, "{:?}", plan.gains_db);
        assert!((plan.gains_db[1] + 0.5).abs() < 0.01, "{:?}", plan.gains_db);
        assert!((plan.gains_db[2] - 0.5).abs() < 0.01, "{:?}", plan.gains_db);
    }

    #[test]
    fn the_correction_is_clamped_and_the_whole_response_stays_within_the_limit() {
        // Every band far off, in directions that pile up where filters overlap.
        for deviation in [
            [-10.0, -10.0, 10.0, 10.0],
            [10.0, -10.0, -10.0, 10.0],
            [-11.0, 11.0, -11.0, 11.0],
        ] {
            let plan = plan_adaptive_eq(&deviated(deviation), SR);
            assert!(plan.gains_db.iter().all(|g| g.abs() <= EQ_LIMIT_DB));
            let peak = eq_response_peak_db(&plan.gains_db, SR);
            assert!(peak <= EQ_LIMIT_DB + 0.01, "{deviation:?}: peak {peak} dB");
        }
    }

    #[test]
    fn a_cut_off_band_gets_nothing_and_does_not_skew_the_others() {
        // A top band 40 dB down is a low-passed source, not a dull one. If it
        // reached the offset, the other three would all look too hot.
        let plan = plan_adaptive_eq(&deviated([0.0, 0.0, 0.0, -40.0]), SR);
        assert_eq!(plan.decisions[3], BandDecision::NoUsableContent);
        assert_eq!(plan.gains_db, [0.0; EQ_BANDS]);
    }

    #[test]
    fn too_few_usable_bands_bypasses_the_stage() {
        // All the energy in one band: what a steady tone looks like.
        let plan = plan_adaptive_eq(&deviated([-60.0, 0.0, -80.0, -100.0]), SR);
        assert_eq!(plan.bypass, Some(EqBypass::TooFewBands));
        assert_eq!(plan.gains_db, [0.0; EQ_BANDS]);
    }

    #[test]
    fn a_band_buried_in_noise_may_be_cut_but_not_boosted() {
        let mut bands = deviated([3.0, 0.0, 0.0, -3.0]);
        // Both the band that wants a cut and the one that wants a boost are
        // only a few dB above the background.
        bands.snr_db[0] = Some(3.0);
        bands.snr_db[3] = Some(3.0);
        let plan = plan_adaptive_eq(&bands, SR);
        assert!(plan.gains_db[0] < 0.0, "{:?}", plan.gains_db);
        assert_eq!(plan.gains_db[3], 0.0);
        assert_eq!(plan.decisions[3], BandDecision::TooNoisyToBoost);
    }

    #[test]
    fn short_lines_are_trusted_less_and_very_short_ones_not_at_all() {
        let gain_at = |secs: f32| {
            let mut bands = deviated([0.0, 2.0, -2.0, 0.0]);
            bands.speech_secs = secs;
            plan_adaptive_eq(&bands, SR)
        };
        let short = gain_at(0.5);
        assert_eq!(short.bypass, Some(EqBypass::TooLittleSpeech));
        assert_eq!(short.gains_db, [0.0; EQ_BANDS]);

        let halfway = gain_at(2.5);
        let full = gain_at(4.0);
        assert!((full.gains_db[2] - 1.0).abs() < 0.01, "{:?}", full.gains_db);
        assert!(
            (halfway.gains_db[2] - 0.5).abs() < 0.01,
            "{:?}",
            halfway.gains_db
        );
    }

    #[test]
    fn a_low_sample_rate_shortens_the_comparison_on_both_sides() {
        // 16 kHz: the 8 kHz row no longer fits, and a measurement that also
        // stops short of it must still read as being on target.
        let target = speech_target_db(8_000.0);
        let full = speech_target_db(24_000.0);
        assert!(target[3].unwrap() < full[3].unwrap());
        let bands = VoiceBands {
            speech_db: target.map(|t| t.map(|t| t - 30.0)),
            snr_db: target.map(|t| t.map(|_| 40.0)),
            speech_secs: 10.0,
        };
        let plan = plan_adaptive_eq(&bands, 16_000);
        assert!(plan.gains_db.iter().all(|g| g.abs() < 0.01));

        // 8 kHz: the top band is gone altogether.
        assert_eq!(speech_target_db(4_000.0)[3], None);
    }

    // -----------------------------------------------------------------------
    // Adaptive EQ: the measurement
    // -----------------------------------------------------------------------

    /// Noise with exactly the reference's spectrum, made in the frequency domain
    /// straight from the table's density. It deliberately shares nothing with
    /// the band integration, so a density compared against a total -- an error
    /// that grows with the width of the band -- cannot cancel out here.
    fn noise_shaped_like_speech(sample_rate: u32, secs: f32, seed: u32) -> Vec<f32> {
        let raw = noise_with_density(sample_rate, secs, seed, reference_density);
        let peak = raw.iter().fold(0f32, |m, s| m.max(s.abs()));
        raw.iter().map(|s| s / peak * 0.3).collect()
    }

    /// The table's density at `f`, in dB per Hz; its ends continue flat.
    fn reference_density(f: f32) -> Option<f32> {
        let row = SPEECH_SPECTRUM
            .iter()
            .find(|(centre, _)| f < third_octave_edges(*centre).1)
            .unwrap_or(&SPEECH_SPECTRUM[SPEECH_SPECTRUM.len() - 1]);
        Some(row.1)
    }

    /// Noise with `density(f)` dB per Hz, or nothing where it returns `None`.
    /// Scaled so that signals of any length with one density have one level.
    fn noise_with_density(
        sample_rate: u32,
        secs: f32,
        seed: u32,
        density: impl Fn(f32) -> Option<f32>,
    ) -> Vec<f32> {
        use rustfft::{num_complex::Complex, FftPlanner};

        let n = ((sample_rate as f32 * secs) as usize).next_power_of_two();
        let bin_hz = sample_rate as f32 / n as f32;
        let phases = xorshift_noise(n / 2 + 1, std::f32::consts::PI, seed);
        let mut spectrum = vec![Complex { re: 0.0, im: 0.0 }; n];
        for k in 1..n / 2 {
            if let Some(level) = density(k as f32 * bin_hz) {
                spectrum[k] = Complex::from_polar(10f32.powf(level / 20.0), phases[k]);
                spectrum[n - k] = spectrum[k].conj();
            }
        }
        FftPlanner::<f32>::new()
            .plan_fft_inverse(n)
            .process(&mut spectrum);
        let scale = 1.0 / (n as f32).sqrt();
        spectrum.iter().map(|c| c.re * scale).collect()
    }

    #[test]
    fn noise_shaped_like_speech_measures_as_being_on_target() {
        let samples = noise_shaped_like_speech(SR, 8.0, 21);
        let bands = measure_voice_bands(&samples, SR).expect("measurable");
        let target = speech_target_db(SR as f32 / 2.0);
        let devs: Vec<f32> = (0..EQ_BANDS)
            .map(|b| target[b].unwrap() - bands.speech_db[b].unwrap())
            .collect();
        let spread = devs.iter().cloned().fold(f32::MIN, f32::max)
            - devs.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 0.5, "deviations {devs:?}");
    }

    #[test]
    fn measuring_a_tilted_take_finds_the_tilt() {
        // The same noise with the top band pushed up through the real filters.
        let mut samples = noise_shaped_like_speech(SR, 8.0, 22);
        let flat = measure_voice_bands(&samples, SR).unwrap();
        apply_adaptive_eq_inplace(&mut samples, SR, 1, &[0.0, 0.0, 0.0, 3.0]);
        let tilted = measure_voice_bands(&samples, SR).unwrap();
        let rise = tilted.speech_db[3].unwrap() - flat.speech_db[3].unwrap();
        let low_rise = tilted.speech_db[0].unwrap() - flat.speech_db[0].unwrap();
        assert!(rise > 2.0, "top band rose {rise} dB");
        assert!(low_rise.abs() < 0.2, "low band moved {low_rise} dB");
    }

    #[test]
    fn the_gate_keeps_quiet_passages_out_of_the_speech_measurement() {
        // Speech-shaped noise, then a much quieter passage of a different shape.
        // Its tilt must not leak into the speech bands, and it must show up as
        // the background.
        let speech = noise_shaped_like_speech(SR, 4.0, 23);
        let hiss: Vec<f32> = xorshift_noise(SR as usize * 2, 0.001, 24);
        let alone = measure_voice_bands(&speech, SR).unwrap();

        let mut mixed = speech.clone();
        mixed.extend_from_slice(&hiss);
        let bands = measure_voice_bands(&mixed, SR).unwrap();

        for b in 0..EQ_BANDS {
            let moved = bands.speech_db[b].unwrap() - alone.speech_db[b].unwrap();
            assert!(moved.abs() < 0.3, "band {b} moved {moved} dB");
            assert!(bands.snr_db[b].unwrap() > EQ_MIN_SNR_DB, "band {b}");
        }
        assert!((bands.speech_secs - alone.speech_secs).abs() < 0.2);
    }

    #[test]
    fn frames_well_under_the_mean_still_count_as_speech() {
        // Unvoiced consonants sit far below the vowels and carry much of the top
        // band. Dropping them would read that band as dull on every real take.
        let loud = noise_shaped_like_speech(SR, 4.0, 28);
        let mut take = loud.clone();
        let consonants: Vec<f32> = noise_shaped_like_speech(SR, 4.0, 29)
            .iter()
            .map(|s| s * 0.1) // -20 dB
            .collect();
        take.extend_from_slice(&consonants);

        let alone = measure_voice_bands(&loud, SR).unwrap();
        let bands = measure_voice_bands(&take, SR).unwrap();
        assert!(
            bands.speech_secs > alone.speech_secs * 1.8,
            "{} s of {} s",
            bands.speech_secs,
            alone.speech_secs
        );
    }

    #[test]
    fn hiss_above_the_measured_range_stops_the_top_band_from_being_boosted() {
        // The high shelf lifts everything above it, but the table stops at 8 kHz.
        // Hiss from 10 kHz up is invisible to the comparison and would still be
        // raised, so the noise check has to look there.
        let speech = noise_with_density(SR, 4.0, 30, reference_density);
        let hiss = noise_with_density(SR, 2.0, 31, |f| (f > 10_000.0).then_some(-7.0));
        let scale = 0.3 / speech.iter().fold(0f32, |m, s| m.max(s.abs()));
        let take: Vec<f32> = speech.iter().chain(&hiss).map(|s| s * scale).collect();

        let bands = measure_voice_bands(&take, SR).unwrap();
        let top = bands.snr_db[EQ_BANDS - 1].unwrap();
        let low = bands.snr_db[0].unwrap();
        assert!(top < EQ_MIN_SNR_DB, "top band {top} dB");
        assert!(low > EQ_MIN_SNR_DB, "low band {low} dB");
    }

    #[test]
    fn a_take_with_no_quiet_frames_falls_back_on_its_quietest_frames() {
        // Steady noise: nothing is 25 dB under the mean, so the background is
        // the quietest 12% of the very same signal, and no band can show enough
        // signal-to-noise for a boost.
        let samples = noise_shaped_like_speech(SR, 8.0, 25);
        let bands = measure_voice_bands(&samples, SR).unwrap();
        for b in 0..EQ_BANDS {
            let snr = bands.snr_db[b].unwrap();
            assert!(snr.abs() < EQ_MIN_SNR_DB, "band {b}: {snr} dB");
        }
    }

    #[test]
    fn digitally_silent_pauses_count_as_a_very_quiet_background() {
        let mut samples = noise_shaped_like_speech(SR, 4.0, 26);
        samples.extend(std::iter::repeat_n(0.0f32, SR as usize));
        let bands = measure_voice_bands(&samples, SR).unwrap();
        for b in 0..EQ_BANDS {
            let snr = bands.snr_db[b].unwrap();
            assert!(snr > EQ_MIN_SNR_DB, "band {b}: {snr} dB");
        }
    }

    #[test]
    fn nothing_to_measure_gives_none() {
        assert_eq!(measure_voice_bands(&[], SR), None);
        assert_eq!(measure_voice_bands(&[0.1; 64], SR), None);
        assert_eq!(measure_voice_bands(&vec![0.0; SR as usize], SR), None);
        assert_eq!(measure_voice_bands(&vec![0.1; SR as usize], 0), None);
    }

    // -----------------------------------------------------------------------
    // Adaptive EQ: the filters
    // -----------------------------------------------------------------------

    #[test]
    fn zero_gains_leave_the_signal_identical_to_the_bit() {
        let original = xorshift_noise(4800, 0.3, 31);
        let mut buf = original.clone();
        apply_adaptive_eq_inplace(&mut buf, SR, 1, &[0.0; EQ_BANDS]);
        assert_eq!(buf, original);
    }

    #[test]
    fn a_gain_reaches_a_tone_in_the_middle_of_its_band() {
        let tone = sine_wave(2000.0, SR, 1.0, 0.1);
        let mut buf = tone.clone();
        apply_adaptive_eq_inplace(&mut buf, SR, 1, &[0.0, 0.0, 1.5, 0.0]);
        let settle = 4800;
        let db = 20.0 * (rms(&buf[settle..]) / rms(&tone[settle..])).log10();
        assert!((db - 1.5).abs() < 0.05, "{db} dB");
    }

    #[test]
    fn equal_gains_sum_to_a_nearly_flat_response() {
        // Bands that overlap too little leave dips between them, too much and
        // they pile up. Either way a broad correction would come out lumpy.
        let filters = eq_filters(&[1.0; EQ_BANDS], SR);
        for freq in [
            60.0, 150.0, 300.0, 600.0, 1000.0, 2000.0, 4000.0, 8000.0, 14000.0,
        ] {
            let db = eq_response_db(&filters, freq, SR);
            println!("{freq:>7} Hz: {db:+.2} dB");
            assert!((0.5..=1.5).contains(&db), "{freq} Hz: {db} dB");
        }
    }

    #[test]
    fn measuring_planning_and_applying_brings_a_coloured_take_closer_to_the_reference() {
        // Speech-shaped noise, made boomy and dull, then a digitally silent
        // pause so that boosts are allowed.
        let mut samples = noise_shaped_like_speech(SR, 8.0, 27);
        apply_adaptive_eq_inplace(&mut samples, SR, 1, &[2.0, 0.0, 0.0, -2.0]);
        samples.extend(std::iter::repeat_n(0.0f32, SR as usize));

        let spread = |samples: &[f32]| {
            let bands = measure_voice_bands(samples, SR).unwrap();
            let target = speech_target_db(SR as f32 / 2.0);
            let devs: Vec<f32> = (0..EQ_BANDS)
                .map(|b| target[b].unwrap() - bands.speech_db[b].unwrap())
                .collect();
            devs.iter().cloned().fold(f32::MIN, f32::max)
                - devs.iter().cloned().fold(f32::MAX, f32::min)
        };

        let before = spread(&samples);
        let plan = plan_adaptive_eq(&measure_voice_bands(&samples, SR).unwrap(), SR);
        assert_eq!(plan.bypass, None);
        assert!(plan.gains_db[0] < 0.0, "{:?}", plan.gains_db);
        assert!(plan.gains_db[3] > 0.0, "{:?}", plan.gains_db);

        apply_adaptive_eq_inplace(&mut samples, SR, 1, &plan.gains_db);
        let after = spread(&samples);
        assert!(after < before * 0.8, "spread {before} dB -> {after} dB");
    }

    #[test]
    fn the_log_line_says_what_happened_to_each_band() {
        let mut bands = deviated([1.0, 0.0, 0.0, -40.0]);
        bands.speech_secs = 6.0;
        let line = plan_adaptive_eq(&bands, SR).describe();
        assert!(line.contains("low -"), "{line}");
        // The band's raw measurement, not only the gain it led to.
        assert!(line.contains("(measured +0.7)"), "{line}");
        assert!(line.contains("high 0 dB, no usable content"), "{line}");
        assert!(line.contains("6.0 s of speech"), "{line}");

        bands.speech_secs = 0.4;
        let line = plan_adaptive_eq(&bands, SR).describe();
        assert!(line.contains("left out"), "{line}");
    }

    #[test]
    fn apply_adaptive_eq_zero_channels_leaves_input_unchanged() {
        // `frames = buf.len() / ch` panics at ch == 0.
        let samples = sine_wave(440.0, SR, 0.1, 0.5);
        let mut buf = samples.clone();
        apply_adaptive_eq_inplace(&mut buf, SR, 0, &[1.5; EQ_BANDS]);
        assert_eq!(buf, samples);
    }

    #[test]
    fn apply_adaptive_eq_is_finite_and_never_fails_at_low_sample_rates() {
        // The old Voice EQ returned an error here: a fixed 10 kHz band is above
        // Nyquist for anything under 20 kHz, which failed the whole file.
        for sample_rate in [8_000, 11_025, 16_000, 22_050] {
            let mut buf = xorshift_noise(sample_rate as usize, 0.3, 11);
            apply_adaptive_eq_inplace(&mut buf, sample_rate, 1, &[1.5, -1.5, 1.5, 1.5]);
            assert!(buf.iter().all(|s| s.is_finite()), "{sample_rate} Hz");
        }
    }

    // -----------------------------------------------------------------------
    // apply_nnnoise
    // -----------------------------------------------------------------------

    #[test]
    fn apply_nnnoise_preserves_length_and_is_finite() {
        let samples = sine_wave(300.0, 48000, 1.0, 0.3);
        let out = apply_nnnoise(&samples, &NnnoiseParams::default()).unwrap();
        assert_eq!(out.len(), samples.len());
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn apply_nnnoise_with_zero_mix_is_an_exact_bypass() {
        // At mix = 0.0 the wet path contributes nothing, so the output must be
        // the input unchanged. This pins the frame-index arithmetic of the
        // streaming implementation: an off-by-one frame in the latency
        // compensation would still preserve length and finiteness, but would
        // no longer reproduce the dry signal exactly.
        let samples = sine_wave(300.0, 48000, 0.5, 0.3);
        let out = apply_nnnoise(&samples, &NnnoiseParams { mix: 0.0 }).unwrap();
        assert_eq!(out, samples);
    }

    #[test]
    fn apply_nnnoise_length_is_exact_for_a_non_multiple_of_frame_size() {
        // FRAME_SIZE is 480; 1000 samples is deliberately not a multiple of it,
        // so the final partial frame exercises the tail clamping.
        let samples = sine_wave(300.0, 48000, 0.0, 0.0);
        assert!(samples.is_empty());
        let ragged = sine_wave(300.0, 48000, 1000.0 / 48000.0, 0.3);
        assert_eq!(ragged.len(), 1000);
        let out = apply_nnnoise(&ragged, &NnnoiseParams::default()).unwrap();
        assert_eq!(out.len(), 1000);
        assert!(out.iter().all(|s| s.is_finite()));
    }
}
