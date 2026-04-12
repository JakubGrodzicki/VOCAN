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

#[cfg(windows)]
use std::os::windows::process::CommandExt;

fn silent_command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd
}

use anyhow::Result;
//use std::sync::Arc;

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
        //let ch_in: Vec<f32> = (0..frames_total).map(|i| samples[i * ch + c]).collect();
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

#[allow(dead_code)]
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

pub fn apply_voice_eq_inplace(
    buf: &mut [f32],
    sample_rate: u32,
    channels: u16,
    strength: f32,
) -> Result<()> {
    use biquad::{Biquad, Coefficients, DirectForm2Transposed, ToHertz, Q_BUTTERWORTH_F32, Type};

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

    // Zbuduj współczynniki raz, z propagacją błędu zamiast unwrap.
    let coeffs: [Coefficients<f32>; 4] = {
        let mut arr: [Option<Coefficients<f32>>; 4] = [None, None, None, None];
        for (i, (t, f, q)) in bands.iter().enumerate() {
            arr[i] = Some(
                Coefficients::<f32>::from_params(*t, fs, f.hz(), *q)
                    .map_err(|e| anyhow::anyhow!("biquad coeffs failed for band {}: {:?}", i, e))?,
            );
        }
        [arr[0].unwrap(), arr[1].unwrap(), arr[2].unwrap(), arr[3].unwrap()]
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

    // Skalowanie do zakresu i16 w miejscu — unikamy osobnej kopii całego sygnału.
    let mut scaled: Vec<f32> = Vec::with_capacity(samples_48k_mono.len());
    for &x in samples_48k_mono {
        scaled.push((x * 32768.0).clamp(-32768.0, 32767.0));
    }

    let total_frames = (scaled.len() + frame_size - 1) / frame_size;
    let mut wet: Vec<f32> = Vec::with_capacity(total_frames * frame_size);
    let mut frame_buf = vec![0f32; frame_size];
    // Reużywany bufor wejściowy — bez alokacji per ramka.
    let mut input_buf = vec![0f32; frame_size];

    for chunk in scaled.chunks(frame_size) {
        input_buf[..chunk.len()].copy_from_slice(chunk);
        if chunk.len() < frame_size {
            input_buf[chunk.len()..].fill(0.0);
        }
        state.process_frame(&mut frame_buf, &input_buf);
        wet.extend_from_slice(&frame_buf);
    }

    // Skalowanie wet z powrotem do ±1.0 in-place (bez drugiego Vec).
    for v in wet.iter_mut() {
        *v /= 32768.0;
    }

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

    // 2. Input WAV (because deep-filter does not read f32le).
    // Tworzymy od razu z rozszerzeniem .wav — NamedTempFile zarządza
    // plikiem RAII-owo, więc przy panice/błędzie zostanie posprzątany.
    let temp_wav_in = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .context("cannot create temp wav input")?;
    let wav_in_path = temp_wav_in.path().to_path_buf();

    let st = silent_command(ffmpeg)
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

    let mut cmd = silent_command(dfn_binary);
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
    let st = silent_command(ffmpeg)
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

    Ok(out_samples)
}

//pub type SharedParams = Arc<SpectralGateParams>;