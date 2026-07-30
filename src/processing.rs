use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::audio_effects;
use crate::ffmpeg::{
    apply_loudnorm_pass2, ffmpeg_cmd, get_duration, get_file_stats, get_file_stats_padded,
    get_sample_rate, measure_peak_dbfs,
};
use crate::types::{LoudnormStats, NormResult, OutputFormat, ProcessingOptions};

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
// Loudness normalization decision logic
// ---------------------------------------------------------------------------

/// Which loudness-normalization strategy to use, carrying whatever measured
/// data is needed to actually apply it.
///
/// Kept separate from [`NormResult`] (the logging-facing summary) because this
/// type also carries the measured [`LoudnormStats`]/gain needed by
/// [`apply_norm_decision`] to build the FFmpeg filter args.
#[derive(Debug, PartialEq)]
pub(crate) enum NormDecision {
    /// Standard 2-pass EBU R128 stats (duration >= 3.0s, first pass succeeded).
    Standard(LoudnormStats),
    /// 2-pass EBU R128 stats measured on a silence-padded copy of the input.
    Padded(LoudnormStats),
    /// Peak-normalization fallback with the computed gain.
    Peak { gain_db: f32 },
    /// No normalization applied.
    Skipped,
}

/// Pure decision: given already-measured (or not-yet-attempted) data, decide
/// which normalization strategy applies. No I/O, no ffmpeg invocation.
///
/// `standard_stats` is only consulted when `duration >= 3.0` -- below that
/// threshold the standard (unpadded) pass is never attempted by the caller,
/// mirroring the real measurement flow (see [`measure_and_decide_normalization`]).
fn decide_normalization(
    duration: f32,
    standard_stats: Option<&LoudnormStats>,
    padded_stats: Option<&LoudnormStats>,
    peak_dbfs: Option<f32>,
    target_peak_dbfs: f32,
) -> NormDecision {
    if duration >= 3.0 {
        if let Some(stats) = standard_stats {
            return NormDecision::Standard(stats.clone());
        }
    }
    if let Some(stats) = padded_stats {
        return NormDecision::Padded(stats.clone());
    }
    match peak_dbfs {
        Some(peak) if peak.is_finite() => {
            let gain_db = target_peak_dbfs - peak;
            if gain_db <= 40.0 {
                NormDecision::Peak { gain_db }
            } else {
                NormDecision::Skipped
            }
        }
        _ => NormDecision::Skipped,
    }
}

/// Measures whatever is needed (standard -> padded -> peak, stopping as soon
/// as one succeeds) and returns the resulting [`NormDecision`]. This is the
/// only place that shells out to ffmpeg for normalization measurement;
/// `decide_normalization` itself stays pure and independently testable.
fn measure_and_decide_normalization(
    input: &Path,
    ffmpeg: &Path,
    target_lufs: f32,
    duration: f32,
    prefix: Option<&str>,
    target_peak_dbfs: f32,
) -> Result<NormDecision> {
    let standard_stats = if duration >= 3.0 {
        get_file_stats(input, ffmpeg, target_lufs, prefix)?
    } else {
        None
    };
    if matches!(
        decide_normalization(
            duration,
            standard_stats.as_ref(),
            None,
            None,
            target_peak_dbfs
        ),
        NormDecision::Standard(_)
    ) {
        return Ok(NormDecision::Standard(standard_stats.unwrap()));
    }

    let pad_secs = f32::max(5.0, duration + 1.0);
    let padded_stats = get_file_stats_padded(input, ffmpeg, target_lufs, pad_secs, prefix)?;
    if matches!(
        decide_normalization(
            duration,
            standard_stats.as_ref(),
            padded_stats.as_ref(),
            None,
            target_peak_dbfs
        ),
        NormDecision::Padded(_)
    ) {
        return Ok(NormDecision::Padded(padded_stats.unwrap()));
    }

    let peak_dbfs = measure_peak_dbfs(input, ffmpeg, prefix)
        .ok()
        .filter(|p| p.is_finite());
    Ok(decide_normalization(
        duration,
        standard_stats.as_ref(),
        padded_stats.as_ref(),
        peak_dbfs,
        target_peak_dbfs,
    ))
}

/// Applies a [`NormDecision`] to the FFmpeg command (pass-2 filter args) and
/// returns the corresponding [`NormResult`] for logging.
fn apply_norm_decision(
    cmd: &mut Command,
    decision: &NormDecision,
    target_lufs: f32,
    source_sr: u32,
    prefix: Option<&str>,
) -> NormResult {
    match decision {
        NormDecision::Standard(stats) => {
            apply_loudnorm_pass2(cmd, target_lufs, stats, source_sr, prefix);
            NormResult::Standard
        }
        NormDecision::Padded(stats) => {
            apply_loudnorm_pass2(cmd, target_lufs, stats, source_sr, prefix);
            NormResult::Padded
        }
        NormDecision::Peak { gain_db } => {
            let vol_filter = format!("volume={:.4}dB", gain_db);
            let filter = match prefix {
                Some(p) => format!("{},{}", p, vol_filter),
                None => vol_filter,
            };
            cmd.args(["-af", &filter]);
            NormResult::Peak { gain_db: *gain_db }
        }
        NormDecision::Skipped => {
            if let Some(p) = prefix {
                cmd.args(["-af", p]);
            }
            NormResult::Skipped
        }
    }
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
        let dfn_name = if cfg!(windows) {
            "deep-filter.exe"
        } else {
            "deep-filter"
        };
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
            processed =
                audio_effects::apply_expander(&processed, 48000, 1, &params, noise_floor_db);
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
        processed =
            audio_effects::apply_nnnoise(&processed, &audio_effects::NnnoiseParams::default())?;
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
        let mut writer =
            hound::WavWriter::create(&temp_wav_path, spec).context("cannot write temp wav")?;
        for &s in &processed {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }

    // 6. Second FFmpeg call: temp WAV → filters (HPF, EQ, compressor) → normalization → encoding
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"])
        .arg(&temp_wav_path)
        .arg("-vn");

    let post_filters = post_deesser_filters();

    // Normalization and encoding — stats measured on the PROCESSED temp WAV.
    let norm_result = if let Some(lufs) = opts.target_lufs {
        let duration = get_duration(&temp_wav_path, ffmpeg).unwrap_or(0.0);
        let decision = measure_and_decide_normalization(
            &temp_wav_path,
            ffmpeg,
            lufs,
            duration,
            Some(&post_filters),
            opts.target_peak_dbfs,
        )?;
        apply_norm_decision(&mut cmd, &decision, lufs, source_sr, Some(&post_filters))
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
    let output = output_base
        .join(rel_path)
        .with_extension(opts.output_format.extension());

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
        let duration = get_duration(input, ffmpeg).unwrap_or(0.0);
        let decision = measure_and_decide_normalization(
            input,
            ffmpeg,
            lufs,
            duration,
            None,
            opts.target_peak_dbfs,
        )?;
        apply_norm_decision(&mut cmd, &decision, lufs, source_sr, None)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> LoudnormStats {
        LoudnormStats {
            input_i: "-23.5".to_string(),
            input_tp: "-6.0".to_string(),
            input_lra: "5.0".to_string(),
            input_thresh: "-33.5".to_string(),
            target_offset: "0.5".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // decide_normalization: boundary-duration table.
    //
    // This pins down the exact behavior introduced by the "Changed padding
    // logic" refactor (commit 8122883): the 1.0s threshold no longer exists
    // (short files always try padded loudnorm first, never jump straight to
    // peak normalization), and the 3.0s threshold gates whether the standard
    // (unpadded) pass is even attempted.
    // -----------------------------------------------------------------------

    #[test]
    fn short_file_with_valid_padded_stats_uses_padded_at_0_9s() {
        let decision = decide_normalization(0.9, None, Some(&stats()), None, -3.0);
        assert_eq!(decision, NormDecision::Padded(stats()));
    }

    #[test]
    fn short_file_with_valid_padded_stats_uses_padded_at_1_0s() {
        // Proves the old 1.0s threshold has no effect anymore: at exactly
        // 1.0s (which used to be the padding/peak boundary) the result is
        // still Padded, purely because padded_stats succeeded.
        let decision = decide_normalization(1.0, None, Some(&stats()), None, -3.0);
        assert_eq!(decision, NormDecision::Padded(stats()));
    }

    #[test]
    fn short_file_with_valid_padded_stats_uses_padded_at_2_9s() {
        let decision = decide_normalization(2.9, None, Some(&stats()), None, -3.0);
        assert_eq!(decision, NormDecision::Padded(stats()));
    }

    #[test]
    fn file_at_3_0s_with_valid_standard_stats_uses_standard() {
        // Boundary is inclusive: duration >= 3.0 attempts standard.
        let decision = decide_normalization(3.0, Some(&stats()), None, None, -3.0);
        assert_eq!(decision, NormDecision::Standard(stats()));
    }

    #[test]
    fn file_at_3_1s_with_valid_standard_stats_uses_standard() {
        let decision = decide_normalization(3.1, Some(&stats()), None, None, -3.0);
        assert_eq!(decision, NormDecision::Standard(stats()));
    }

    #[test]
    fn file_at_3_1s_falls_back_to_padded_when_standard_measurement_failed() {
        let decision = decide_normalization(3.1, None, Some(&stats()), None, -3.0);
        assert_eq!(decision, NormDecision::Padded(stats()));
    }

    #[test]
    fn falls_back_to_peak_when_both_standard_and_padded_fail() {
        let decision = decide_normalization(5.0, None, None, Some(-6.0), -3.0);
        assert_eq!(decision, NormDecision::Peak { gain_db: 3.0 });
    }

    #[test]
    fn peak_gain_exceeding_40db_cap_is_skipped() {
        // target=-3.0, peak=-50.0 => gain_db = 47.0 > 40.0 cap.
        let decision = decide_normalization(5.0, None, None, Some(-50.0), -3.0);
        assert_eq!(decision, NormDecision::Skipped);
    }

    #[test]
    fn missing_peak_measurement_is_skipped() {
        let decision = decide_normalization(5.0, None, None, None, -3.0);
        assert_eq!(decision, NormDecision::Skipped);
    }

    #[test]
    fn extremely_short_file_still_prefers_padded_over_peak_when_padded_succeeds() {
        // The exact case that changed in the refactor: pre-refactor code
        // never attempted padded loudnorm below 1.0s. Post-refactor, padded
        // is tried unconditionally for any duration < 3.0s.
        let decision = decide_normalization(0.1, None, Some(&stats()), Some(-1.0), -3.0);
        assert_eq!(decision, NormDecision::Padded(stats()));
    }

    // -----------------------------------------------------------------------
    // apply_norm_decision: Peak/Skipped build the expected -af filter args
    // (Standard/Padded delegate filter-building to apply_loudnorm_pass2,
    // covered separately in ffmpeg.rs).
    // -----------------------------------------------------------------------

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn apply_peak_decision_adds_volume_filter_with_prefix() {
        let mut cmd = Command::new("ffmpeg");
        let result = apply_norm_decision(
            &mut cmd,
            &NormDecision::Peak { gain_db: 2.5 },
            -16.0,
            44100,
            Some("highpass=f=70"),
        );
        assert_eq!(result, NormResult::Peak { gain_db: 2.5 });
        let args = args_of(&cmd);
        assert_eq!(args, vec!["-af", "highpass=f=70,volume=2.5000dB"]);
    }

    #[test]
    fn apply_peak_decision_adds_volume_filter_without_prefix() {
        let mut cmd = Command::new("ffmpeg");
        apply_norm_decision(
            &mut cmd,
            &NormDecision::Peak { gain_db: -1.0 },
            -16.0,
            44100,
            None,
        );
        assert_eq!(args_of(&cmd), vec!["-af", "volume=-1.0000dB"]);
    }

    #[test]
    fn apply_skipped_decision_keeps_prefix_filters_only() {
        let mut cmd = Command::new("ffmpeg");
        let result = apply_norm_decision(
            &mut cmd,
            &NormDecision::Skipped,
            -16.0,
            44100,
            Some("highpass=f=70"),
        );
        assert_eq!(result, NormResult::Skipped);
        assert_eq!(args_of(&cmd), vec!["-af", "highpass=f=70"]);
    }

    #[test]
    fn apply_skipped_decision_without_prefix_adds_no_args() {
        let mut cmd = Command::new("ffmpeg");
        apply_norm_decision(&mut cmd, &NormDecision::Skipped, -16.0, 44100, None);
        assert!(args_of(&cmd).is_empty());
    }
}
