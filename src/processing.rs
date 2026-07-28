use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

use crate::audio_effects;
use crate::ffmpeg::{
    apply_loudnorm_pass2, ffmpeg_cmd, get_duration, get_file_stats, get_file_stats_padded,
    get_sample_rate, measure_peak_dbfs,
};
use crate::types::{NormResult, OutputFormat, ProcessingOptions};

// ---------------------------------------------------------------------------
// Automixer filter chains
// ---------------------------------------------------------------------------

/// Returns **only** the de-esser filter (used in the new pipeline).
fn deesser_only_filter() -> String {
    "deesser=i=0.4:m=0.5:f=0.5:s=o".to_string()
}

/// Returns filters after the de-esser: HPF, EQ, compressor (no de-esser).
fn post_deesser_filters() -> String {
    let hpf = "highpass=f=70:poles=2:width_type=q:width=1.0,highpass=f=70:poles=1";
    let eq_90 = "equalizer=f=90:width_type=q:width=2.478:g=-2.0";
    let eq_175 = "equalizer=f=175:width_type=q:width=1.0:g=-2.22";
    let eq_360 = "equalizer=f=360:width_type=q:width=1.0:g=-1.23";
    let eq_1350 = "equalizer=f=1350:width_type=q:width=1.4:g=1.4";
    let eq_4246 = "equalizer=f=4246:width_type=q:width=2.0:g=-1.36";
    let shelf_8k = "highshelf=f=8000:width_type=q:width=1.0:g=1.0";
    let comp = "acompressor=threshold=0.251:ratio=4:attack=5:release=80:makeup=4";

    format!(
        "{},{},{},{},{},{},{},{}",
        hpf, eq_90, eq_175, eq_360, eq_1350, eq_4246, shelf_8k, comp
    )
}

// ---------------------------------------------------------------------------
// Output format helpers
// ---------------------------------------------------------------------------

/// Adds codec and bitrate arguments to the FFmpeg command based on the output format.
fn add_format_args(cmd: &mut std::process::Command, format: OutputFormat, bitrate_kbps: u32) {
    cmd.args(["-c:a", format.ffmpeg_codec()]);
    if format.needs_bitrate() {
        cmd.args(["-b:a", &format!("{}k", bitrate_kbps)]);
    }
}

// ---------------------------------------------------------------------------
// New pipeline with Rust DSP (when automixer + new modules are active)
// ---------------------------------------------------------------------------

/// Processes the file using Rust DSP (SG/NN + Voice EQ) between the de-esser and the rest of the chain.
///
/// **Loudnorm correctness:** All loudness/peak measurements (pass-1 stats) are
/// performed on the DSP-processed audio (written to a temp WAV file), not the
/// original input. This ensures the linear normalization in pass-2 operates on
/// the same signal that was measured, producing correct target loudness.
fn process_with_rust_dsp(
    input: &Path,
    output: &Path,
    opts: &ProcessingOptions,
    ffmpeg: &Path,
) -> Result<NormResult> {
    // 1. Get original sample rate (to restore later)
    let source_sr = get_sample_rate(input, ffmpeg).unwrap_or(44100);

    // 2+3. FFmpeg de-esser → stdout (f32le) → memory (no temp file)
    let mut child = ffmpeg_cmd(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &deesser_only_filter()])
        .args(["-ac", "1", "-ar", "48000", "-f", "f32le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("FFmpeg spawn failed (de-esser pass)")?;

    // Read stderr in a separate thread to avoid blocking FFmpeg
    // when there is large diagnostic output.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let mut raw = Vec::with_capacity(1 << 20);
    child
        .stdout
        .as_mut()
        .context("FFmpeg stdout not piped")?
        .read_to_end(&mut raw)?;
    let status = child.wait().context("FFmpeg de-esser wait failed")?;
    let stderr_text = stderr_handle.join().unwrap_or_default();
    if !status.success() {
        return Err(anyhow!("FFmpeg de-esser pass failed: {}", stderr_text));
    }

    let samples: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    // 4. Rust DSP
    let mut processed = samples;

    // --- Noise floor estimation (BEFORE dereverb) ---
    // The spec requires analysis on the raw/pre-dereverb signal — denoising
    // distorts the floor characteristic and biases the estimate.
    // We run it on the post-de-esser samples (the earliest point in Rust memory);
    // the de-esser is a subtle HF filter that won't materially affect broadband
    // RMS noise-floor estimation.
    let expander_noise_floor = if opts.automixer_expander {
        audio_effects::estimate_noise_floor_db(&processed, 48000)
    } else {
        None
    };

    // Dereverb FIRST — reverb tails would otherwise confuse the gate
    // and smear bands that the EQ would later emphasize.
    if opts.automixer_dfn3_dereverb {
        let params = audio_effects::DereverbParams {
            mix: opts.automixer_dfn3_mix,
            attenuation_limit: 30.0,
            post_filter: opts.automixer_dfn3_postfilter,
        };
        // Look for deep-filter binary next to ffmpeg, or next to our exe.
        let dfn_name = if cfg!(windows) { "deep-filter.exe" } else { "deep-filter" };
        let dfn_path = ffmpeg
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(dfn_name))
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join(dfn_name)))
            })
            .ok_or_else(|| anyhow!("cannot locate {}", dfn_name))?;
        processed = audio_effects::apply_dereverb_dfn3(&processed, &params, &dfn_path, ffmpeg)?;
    }

    // --- Downward Expander (after dereverb, before spectral gate / nnnoise) ---
    // Uses the noise floor estimated on the pre-dereverb signal.
    // If estimation returned None (file too short / too little low-level content),
    // the stage is silently bypassed for this file.
    if opts.automixer_expander {
        if let Some(noise_floor_db) = expander_noise_floor {
            let params = audio_effects::ExpanderParams {
                safety_pct: opts.automixer_expander_safety_pct,
                reduction_profile: opts.automixer_expander_reduction_profile,
                ..Default::default()
            };
            processed = audio_effects::apply_expander(&processed, 48000, 1, &params, noise_floor_db);
        }
    }

    // Denoise: spectral gate OR nnnoise (mutually exclusive)
    if opts.automixer_spectral_gate {
        processed = audio_effects::apply_spectral_gate(
            &processed,
            48000,
            1,
            &audio_effects::SpectralGateParams::default(),
        )?;
    } else if opts.automixer_nn_dereverb {
        processed = audio_effects::apply_nnnoise(
            &processed,
            &audio_effects::NnnoiseParams::default(),
        )?;
    }

    // Voice EQ always at 50% strength — LAST in the chain (in-place)
    audio_effects::apply_voice_eq_inplace(&mut processed, 48000, 1, 0.5)?;

    // 5. Write processed samples to a temp WAV file using hound.
    //    This file is used for BOTH loudnorm pass-1 measurement AND as the
    //    input to the final FFmpeg encoding pass, ensuring the signal measured
    //    is the same signal that gets normalized and encoded.
    let temp_wav = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .context("cannot create temp wav for DSP output")?;
    let temp_wav_path = temp_wav.path().to_path_buf();
    {
        let spec = hound::WavSpec {
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
            channels: 1,
        };
        let mut writer = hound::WavWriter::create(&temp_wav_path, spec)
            .context("cannot write temp wav")?;
        for &s in &processed {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }

    // 6. Second FFmpeg call: temp WAV → filters (HPF, EQ, compressor) → normalization → encoding
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"]).arg(&temp_wav_path).arg("-vn");

    let post_filters = post_deesser_filters();

    // Normalization and encoding — stats measured on the PROCESSED temp WAV.
    let norm_result = if let Some(lufs) = opts.target_lufs {
        match get_file_stats(&temp_wav_path, ffmpeg, lufs, Some(&post_filters))? {
            Some(stats) => {
                apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, Some(&post_filters));
                NormResult::Standard
            }
            None => {
                let duration = get_duration(&temp_wav_path, ffmpeg).unwrap_or(0.0);
                if duration >= 1.0 {
                    let pad_secs = f32::max(5.0, duration + 1.0);
                    match get_file_stats_padded(
                        &temp_wav_path, ffmpeg, lufs, pad_secs, Some(&post_filters),
                    )? {
                        Some(stats) => {
                            apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, Some(&post_filters));
                            NormResult::Padded
                        }
                        None => {
                            cmd.args(["-af", &post_filters]);
                            NormResult::Skipped
                        }
                    }
                } else {
                    match measure_peak_dbfs(&temp_wav_path, ffmpeg, Some(&post_filters)) {
                        Ok(peak_dbfs) if peak_dbfs.is_finite() => {
                            let gain_db = opts.target_peak_dbfs - peak_dbfs;
                            if gain_db <= 40.0 {
                                let vol_filter = format!("volume={:.4}dB", gain_db);
                                let filter = format!("{},{}", post_filters, vol_filter);
                                cmd.args(["-af", &filter]);
                                NormResult::Peak { gain_db }
                            } else {
                                cmd.args(["-af", &post_filters]);
                                NormResult::Skipped
                            }
                        }
                        _ => {
                            cmd.args(["-af", &post_filters]);
                            NormResult::Skipped
                        }
                    }
                }
            }
        }
    } else {
        cmd.args(["-af", &post_filters]);
        NormResult::Skipped
    };

    add_format_args(&mut cmd, opts.output_format, opts.bitrate_kbps);
    cmd.arg(output);

    let final_output = cmd
        .stderr(Stdio::piped())
        .output()
        .context("Failed to run FFmpeg (Conversion)")?;

    if !final_output.status.success() {
        let err = String::from_utf8_lossy(&final_output.stderr);
        return Err(anyhow!("FFmpeg Error: {}", err));
    }

    Ok(norm_result)
}

// ---------------------------------------------------------------------------
// Main file processing function
// ---------------------------------------------------------------------------

pub fn process_single_file(
    input: &Path,
    input_base: &Path,
    output_base: &Path,
    opts: &ProcessingOptions,
    ffmpeg: &Path,
) -> Result<NormResult> {
    let rel_path = input.strip_prefix(input_base)?;
    let output = output_base.join(rel_path).with_extension(opts.output_format.extension());

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // If automixer is enabled, use the new pipeline with Rust DSP.
    // Otherwise, use old logic (no additional modules).
    if opts.automixer {
        return process_with_rust_dsp(input, &output, opts, ffmpeg);
    }

    // Old pipeline (without automixer)
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"]).arg(input).arg("-vn");

    let norm_result = if let Some(lufs) = opts.target_lufs {
        let source_sr = get_sample_rate(input, ffmpeg).unwrap_or(44100);
        match get_file_stats(input, ffmpeg, lufs, None)? {
            Some(stats) => {
                apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, None);
                NormResult::Standard
            }
            None => {
                let duration = get_duration(input, ffmpeg).unwrap_or(0.0);
                if duration >= 1.0 {
                    let pad_secs = f32::max(5.0, duration + 1.0);
                    match get_file_stats_padded(input, ffmpeg, lufs, pad_secs, None)? {
                        Some(stats) => {
                            apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, None);
                            NormResult::Padded
                        }
                        None => NormResult::Skipped,
                    }
                } else {
                    match measure_peak_dbfs(input, ffmpeg, None) {
                        Ok(peak_dbfs) if peak_dbfs.is_finite() => {
                            let gain_db = opts.target_peak_dbfs - peak_dbfs;
                            if gain_db <= 40.0 {
                                let vol_filter = format!("volume={:.4}dB", gain_db);
                                cmd.args(["-af", &vol_filter]);
                                NormResult::Peak { gain_db }
                            } else {
                                NormResult::Skipped
                            }
                        }
                        _ => NormResult::Skipped,
                    }
                }
            }
        }
    } else {
        NormResult::Skipped
    };

    add_format_args(&mut cmd, opts.output_format, opts.bitrate_kbps);
    cmd.arg(&output);

    let final_output = cmd
        .stderr(Stdio::piped())
        .output()
        .context("Failed to run FFmpeg (Conversion)")?;

    if !final_output.status.success() {
        let err = String::from_utf8_lossy(&final_output.stderr);
        return Err(anyhow!("FFmpeg Error: {}", err));
    }

    Ok(norm_result)
}