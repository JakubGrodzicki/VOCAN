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
use std::path::Path;
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

    let threshold_lin = 10f32.powf(p.threshold_db / 20.0);
    let ratio = p.ratio.max(1.0);
    let frame_time = hop as f32 / sample_rate as f32;
    let a_coef = (-frame_time / p.attack_s).exp();
    let r_coef = (-frame_time / p.release_s).exp();

    let ch = channels as usize;
    let frames_total = samples.len() / ch;
    let mut out = vec![0f32; samples.len()];

    let mut buf: Vec<Complex<f32>> = vec![Complex { re: 0.0, im: 0.0 }; fft_size];

    for c in 0..ch {
        let mut ch_out = vec![0f32; frames_total];
        let mut bin_env = vec![0f32; fft_size / 2 + 1];

        let mut pos = 0usize;
        while pos + fft_size <= frames_total {
            for n in 0..fft_size {
                buf[n].re = samples[(pos + n) * ch + c] * window[n];
                buf[n].im = 0.0;
            }
            fft.process(&mut buf);

            for k in 0..=fft_size / 2 {
                let mag = (buf[k].re * buf[k].re + buf[k].im * buf[k].im).sqrt();
                let target = mag;
                let env = &mut bin_env[k];
                let coef = if target > *env { a_coef } else { r_coef };
                *env = target + coef * (*env - target);

                let gain = if *env < threshold_lin {
                    (*env / threshold_lin).powf(ratio - 1.0).max(0.0)
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

            ifft.process(&mut buf);
            let norm = 1.0 / fft_size as f32;
            for n in 0..fft_size {
                ch_out[pos + n] += buf[n].re * norm * window[n];
            }
            pos += hop;
        }

        let comp = 2.0 / 3.0;
        for i in 0..frames_total {
            out[i * ch + c] = ch_out[i] * comp;
        }
    }
    Ok(out)
}

// ===========================================================================
// MODULE 2: Voice EQ (static biquad bank)
// ===========================================================================

pub fn apply_voice_eq_inplace(
    buf: &mut [f32],
    sample_rate: u32,
    channels: u16,
    strength: f32,
) -> Result<()> {
    use biquad::{Biquad, Coefficients, DirectForm2Transposed, ToHertz, Type, Q_BUTTERWORTH_F32};

    let fs = (sample_rate as f32).hz();
    let s = strength.clamp(0.0, 1.0);
    let bands: [(Type<f32>, f32, f32); 4] = [
        (Type::LowShelf(-3.0 * s), 120.0, Q_BUTTERWORTH_F32),
        (Type::PeakingEQ(-1.5 * s), 400.0, 1.0),
        (Type::PeakingEQ(2.0 * s), 2500.0, 1.2),
        (Type::HighShelf(1.5 * s), 10000.0, Q_BUTTERWORTH_F32),
    ];

    let ch = channels as usize;
    let frames = buf.len() / ch;

    // Build coefficients once, propagating errors instead of unwrap.
    let coeffs: [Coefficients<f32>; 4] = {
        let mut arr: [Option<Coefficients<f32>>; 4] = [None, None, None, None];
        for (i, (t, f, q)) in bands.iter().enumerate() {
            arr[i] = Some(
                Coefficients::<f32>::from_params(*t, fs, f.hz(), *q)
                    .map_err(|e| anyhow::anyhow!("biquad coeffs failed for band {}: {:?}", i, e))?,
            );
        }
        [
            arr[0].unwrap(),
            arr[1].unwrap(),
            arr[2].unwrap(),
            arr[3].unwrap(),
        ]
    };

    for c in 0..ch {
        let mut filters: [DirectForm2Transposed<f32>; 4] =
            std::array::from_fn(|i| DirectForm2Transposed::<f32>::new(coeffs[i]));
        for i in 0..frames {
            let idx = i * ch + c;
            let mut x = buf[idx];
            for f in filters.iter_mut() {
                x = <DirectForm2Transposed<f32> as Biquad<f32>>::run(f, x);
            }
            buf[idx] = x;
        }
    }
    Ok(())
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

    // Scale to i16 range in-place — avoids a separate copy of the entire signal.
    let mut scaled: Vec<f32> = Vec::with_capacity(samples_48k_mono.len());
    for &x in samples_48k_mono {
        scaled.push((x * 32768.0).clamp(-32768.0, 32767.0));
    }

    let total_frames = scaled.len().div_ceil(frame_size);
    let mut wet: Vec<f32> = Vec::with_capacity(total_frames * frame_size);
    let mut frame_buf = vec![0f32; frame_size];
    // Reusable input buffer — no per-frame allocation.
    let mut input_buf = vec![0f32; frame_size];

    for chunk in scaled.chunks(frame_size) {
        input_buf[..chunk.len()].copy_from_slice(chunk);
        if chunk.len() < frame_size {
            input_buf[chunk.len()..].fill(0.0);
        }
        state.process_frame(&mut frame_buf, &input_buf);
        wet.extend_from_slice(&frame_buf);
    }

    // Scale wet back to ±1.0 in-place (no second Vec).
    for v in wet.iter_mut() {
        *v /= 32768.0;
    }

    let shift = frame_size;
    let dry_len = samples_48k_mono.len();
    let mut out = Vec::with_capacity(dry_len);

    for i in 0..dry_len {
        let dry = samples_48k_mono[i];
        let wet_sample = if i + shift < wet.len() {
            wet[i + shift]
        } else {
            0.0
        };
        out.push(dry * (1.0 - mix) + wet_sample * mix);
    }

    Ok(out)
}

// ===========================================================================
// MODULE 4: Dereverb via DeepFilterNet3 CLI (subprocess)
// ===========================================================================
//
// Requires `deep-filter` binary (https://github.com/Rikorose/DeepFilterNet/releases)
// alongside ffmpeg. Expects 48 kHz mono f32 signal. Returns the same.
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
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
        hound::SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / max_val)
                .collect()
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
    let temp_wav_in = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .context("cannot create temp wav input")?;
    let wav_in_path = temp_wav_in.path().to_path_buf();
    write_wav_f32(&wav_in_path, samples_48k_mono)?;

    // 2. DeepFilterNet3 inference.
    // We use a dedicated temp directory and take the first produced WAV
    // instead of guessing the naming scheme (which differs between CLI builds).
    let out_dir = tempfile::tempdir().context("cannot create temp dir for deep-filter")?;

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

    let out = cmd.output().context("deep-filter invocation failed")?;
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

    if win_len == 0 || samples.len() < win_len {
        return None;
    }

    // Hann window for RMS calculation.
    let window: Vec<f32> = (0..win_len)
        .map(|n| {
            let x = n as f32 / (win_len - 1) as f32;
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
        })
        .collect();

    // Collect RMS values (in dB) for all windows.
    let mut rms_db_values: Vec<f32> = Vec::new();
    let mut pos = 0usize;

    while pos + win_len <= samples.len() {
        let mut sum_sq = 0.0f32;
        for n in 0..win_len {
            let s = samples[pos + n] * window[n];
            sum_sq += s * s;
        }
        let rms = (sum_sq / win_len as f32).sqrt();

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

    // Sort and take p12 (12th percentile).
    rms_db_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (rms_db_values.len() as f32 * 0.12).round() as usize;
    let idx = idx.min(rms_db_values.len() - 1);

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
/// Returns a new `Vec<f32>` — infallible (no I/O, no external calls).
pub fn apply_expander(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    params: &ExpanderParams,
    noise_floor_db: f32,
) -> Vec<f32> {
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return samples.to_vec();
    }

    let ch = channels as usize;
    let frames = samples.len() / ch;

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

    let mut out = vec![0.0f32; samples.len()];

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
            let sample = samples[i * ch + c];

            // --- RMS detector (circular buffer) ---
            let old_sq = rms_buf[rms_buf_idx];
            let new_sq = sample * sample;
            rms_buf[rms_buf_idx] = new_sq;
            rms_buf_idx = (rms_buf_idx + 1) % rms_win;
            rms_sum_sq = rms_sum_sq - old_sq + new_sq;
            let rms_sum_sq = rms_sum_sq.max(0.0); // guard against float drift
            let rms = (rms_sum_sq / rms_win as f32).sqrt();
            let det_db = if rms > 1e-10 {
                20.0 * rms.log10()
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
                10.0f32.powf(gain_db / 20.0)
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
            out[i * ch + c] = sample * gain_lin;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn estimate_noise_floor_db_on_pure_silence_returns_none() {
        let samples = vec![0.0f32; 96000]; // 2s at 48kHz, well above the length floor
        assert_eq!(estimate_noise_floor_db(&samples, 48000), None);
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
    fn apply_expander_on_empty_signal_returns_empty() {
        let params = ExpanderParams::default();
        assert!(apply_expander(&[], 48000, 1, &params, -40.0).is_empty());
    }

    #[test]
    fn apply_expander_zero_channels_returns_input_unchanged() {
        let samples = sine_wave(440.0, 48000, 0.1, 0.5);
        let params = ExpanderParams::default();
        let out = apply_expander(&samples, 48000, 0, &params, -40.0);
        assert_eq!(out, samples);
    }

    #[test]
    fn apply_expander_never_produces_nan_or_inf() {
        let mut samples = xorshift_noise(48000, 0.3, 7);
        samples.extend(vec![0.0f32; 4800]); // silence segment
        samples.extend(sine_wave(880.0, 48000, 0.5, 0.8));
        let params = ExpanderParams::default();
        let out = apply_expander(&samples, 48000, 1, &params, -40.0);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn apply_expander_loud_signal_stays_near_unity_gain_after_attack() {
        // A sine well above the noise floor for its whole duration; after the
        // ~10ms default attack settles, gain should be close to 1.0.
        let sr = 48000u32;
        let samples = sine_wave(440.0, sr, 1.0, 0.5);
        let params = ExpanderParams::default();
        let out = apply_expander(&samples, sr, 1, &params, -40.0);

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
        let out = apply_expander(&samples, sr, 1, &params, -20.0);

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
    fn apply_spectral_gate_shorter_than_fft_window_returns_zeros_same_length() {
        let samples = xorshift_noise(100, 0.2, 3); // shorter than fft_size (2048)
        let out = apply_spectral_gate(&samples, 48000, 1, &SpectralGateParams::default()).unwrap();
        assert_eq!(out.len(), samples.len());
        assert!(out.iter().all(|s| *s == 0.0));
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
    // apply_voice_eq_inplace
    // -----------------------------------------------------------------------

    #[test]
    fn apply_voice_eq_inplace_zero_strength_is_near_identity() {
        // At strength=0.0 all bands are configured for 0dB gain, which for
        // shelving/peaking biquads is a true allpass (unity magnitude
        // response) -- RMS should be preserved after the initial transient.
        let original = sine_wave(300.0, 48000, 1.0, 0.4);
        let mut buf = original.clone();
        apply_voice_eq_inplace(&mut buf, 48000, 1, 0.0).unwrap();
        assert!(buf.iter().all(|s| s.is_finite()));

        let settle = 500; // skip filter startup transient
        let ratio = rms(&buf[settle..]) / rms(&original[settle..]);
        assert!((0.9..=1.1).contains(&ratio), "ratio={ratio}");
    }

    #[test]
    fn apply_voice_eq_inplace_full_strength_stays_finite() {
        let mut buf = xorshift_noise(48000, 0.3, 11);
        apply_voice_eq_inplace(&mut buf, 48000, 1, 1.0).unwrap();
        assert!(buf.iter().all(|s| s.is_finite()));
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
}
