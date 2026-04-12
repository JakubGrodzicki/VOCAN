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

use anyhow::Result;
use std::sync::Arc;

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
        Self { threshold_db: -45.0, ratio: 2.0, attack_s: 0.002, release_s: 0.080 }
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
        let ch_in: Vec<f32> = (0..frames_total).map(|i| samples[i * ch + c]).collect();
        let mut ch_out = vec![0f32; frames_total];
        let mut bin_env = vec![0f32; fft_size / 2 + 1];

        let mut pos = 0usize;
        while pos + fft_size <= ch_in.len() {
            for n in 0..fft_size {
                buf[n].re = ch_in[pos + n] * window[n];
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
                    buf[fft_size - k] = Complex { re: buf[k].re, im: -buf[k].im };
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

pub fn apply_voice_eq(samples: &[f32], sample_rate: u32, channels: u16, strength: f32) -> Result<Vec<f32>> {
    use biquad::{Biquad, Coefficients, DirectForm2Transposed, ToHertz, Q_BUTTERWORTH_F32, Type};

    let fs = (sample_rate as f32).hz();
    let s = strength.clamp(0.0, 1.0);

    let bands: Vec<(Type<f32>, f32, f32)> = vec![
        (Type::LowShelf(-3.0 * s), 120.0, Q_BUTTERWORTH_F32),
        (Type::PeakingEQ(-1.5 * s), 400.0, 1.0),
        (Type::PeakingEQ(2.0 * s), 2500.0, 1.2),
        (Type::HighShelf(1.5 * s), 10000.0, Q_BUTTERWORTH_F32),
    ];

    let ch = channels as usize;
    let mut out = samples.to_vec();

    for c in 0..ch {
        let mut filters: Vec<DirectForm2Transposed<f32>> = bands
            .iter()
            .map(|(t, f, q)| {
                let coeffs = Coefficients::<f32>::from_params(*t, fs, f.hz(), *q).unwrap();
                DirectForm2Transposed::<f32>::new(coeffs)
            })
            .collect();

        let frames = out.len() / ch;
        for i in 0..frames {
            let idx = i * ch + c;
            let mut x = out[idx];
            for f in filters.iter_mut() {
                x = f.run(x);
            }
            out[idx] = x;
        }
    }
    Ok(out)
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

    let scaled: Vec<f32> = samples_48k_mono
        .iter()
        .map(|&x| (x * 32768.0).clamp(-32768.0, 32767.0))
        .collect();

    let total_frames = (scaled.len() + frame_size - 1) / frame_size;
    let mut wet = Vec::with_capacity(total_frames * frame_size);
    let mut frame_buf = vec![0f32; frame_size];

    for chunk in scaled.chunks(frame_size) {
        let input: Vec<f32> = if chunk.len() == frame_size {
            chunk.to_vec()
        } else {
            let mut v = chunk.to_vec();
            v.resize(frame_size, 0.0);
            v
        };
        state.process_frame(&mut frame_buf, &input);
        wet.extend_from_slice(&frame_buf);
    }

    let wet: Vec<f32> = wet.into_iter().map(|x| x / 32768.0).collect();

    let shift = frame_size;
    let dry_len = samples_48k_mono.len();
    let mut out = Vec::with_capacity(dry_len);

    for i in 0..dry_len {
        let dry = samples_48k_mono[i];
        let wet_sample = if i + shift < wet.len() { wet[i + shift] } else { 0.0 };
        out.push(dry * (1.0 - mix) + wet_sample * mix);
    }

    Ok(out)
}

// ===========================================================================
// MODULE 4: Dereverb via DeepFilterNet3 CLI (subprocess)
// ===========================================================================
//
// Requires `deep-filter.exe` binary (https://github.com/Rikorose/DeepFilterNet/releases)
// alongside ffmpeg.exe. Expects 48 kHz mono f32 signal. Returns the same.
//
// DeepFilterNet3 performs joint denoise + dereverb at 48 kHz — no resampling required.
// The -D flag compensates for STFT latency and model lookahead (sample-accurate),
// so wet and dry are synchronized without additional shifting.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

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
        Self { mix: 0.8, attenuation_limit: 30.0, post_filter: false }
    }
}

pub fn apply_dereverb_dfn3(
    samples_48k_mono: &[f32],
    params: &DereverbParams,
    dfn_binary: &Path,
    ffmpeg: &Path,
) -> Result<Vec<f32>> {
    use anyhow::Context;
    use tempfile::NamedTempFile;

    if !dfn_binary.exists() {
        anyhow::bail!("deep-filter binary not found at: {}", dfn_binary.display());
    }

    // deep-filter operates on WAV files. Pipeline:
    // f32le -> (ffmpeg) -> wav -> (deep-filter) -> wav -> (ffmpeg) -> f32le.

    // 1. Input f32le (raw dump from our DSP)
    let temp_raw_in = NamedTempFile::new()?;
    {
        let mut f = std::fs::File::create(temp_raw_in.path())?;
        for s in samples_48k_mono {
            f.write_all(&s.to_le_bytes())?;
        }
    }

    // 2. Input WAV (because deep-filter does not read f32le)
    let temp_wav_in = NamedTempFile::new()?;
    let wav_in_path = temp_wav_in.path().with_extension("wav");

    let st = Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-f", "f32le", "-ar", "48000", "-ac", "1", "-i"])
        .arg(temp_raw_in.path())
        .arg(&wav_in_path)
        .stderr(Stdio::null())
        .status()
        .context("ffmpeg f32le->wav failed")?;
    if !st.success() {
        anyhow::bail!("ffmpeg f32le->wav failed");
    }

    // 3. DeepFilterNet3 Inference.
    // We use a dedicated temp directory and take the first produced WAV
    // instead of guessing the naming scheme (which differs between CLI builds).
    let out_dir = tempfile::tempdir().context("cannot create temp dir for deep-filter")?;

    let mut cmd = Command::new(dfn_binary);
    cmd.arg("-D")
        .args(["-a", &format!("{:.1}", params.attenuation_limit.clamp(0.0, 100.0))])
        .arg("-o")
        .arg(out_dir.path())
        .arg(&wav_in_path);
    if params.post_filter {
        cmd.arg("--pf");
    }

    let out = cmd
        .output()
        .context("deep-filter invocation failed")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("deep-filter returned non-zero: {}", stderr);
    }

    // 4. Find output file (there should be exactly one WAV in out_dir).
    let wav_out_path = std::fs::read_dir(out_dir.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|x| x == "wav").unwrap_or(false))
        .context("deep-filter produced no WAV output")?;

    // 5. WAV -> f32le
    let temp_raw_out = NamedTempFile::new()?;
    let st = Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(&wav_out_path)
        .args(["-f", "f32le", "-ar", "48000", "-ac", "1"])
        .arg(temp_raw_out.path())
        .stderr(Stdio::null())
        .status()
        .context("ffmpeg wav->f32le failed")?;
    if !st.success() {
        anyhow::bail!("ffmpeg wav->f32le failed");
    }

    let mut raw = Vec::new();
    std::fs::File::open(temp_raw_out.path())?.read_to_end(&mut raw)?;
    let wet: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    // 6. Dry/wet mix. With -D, wet is already sample-accurate synchronized.
    let mix = params.mix.clamp(0.0, 1.0);
    let n = samples_48k_mono.len().min(wet.len());
    let mut out_samples = Vec::with_capacity(samples_48k_mono.len());
    for i in 0..n {
        out_samples.push(samples_48k_mono[i] * (1.0 - mix) + wet[i] * mix);
    }
    // If wet was shorter (it shouldn't be), fill with dry.
    for i in n..samples_48k_mono.len() {
        out_samples.push(samples_48k_mono[i]);
    }

    // Cleanup: wav_in_path is not managed by NamedTempFile (we changed the
    // extension), so we remove it manually. out_dir and everything inside 
    // will be cleaned up automatically on drop.
    let _ = std::fs::remove_file(&wav_in_path);

    Ok(out_samples)
}

pub type SharedParams = Arc<SpectralGateParams>;