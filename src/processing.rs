use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use tempfile::NamedTempFile;

use crate::audio_effects;
use crate::ffmpeg::{
    apply_loudnorm_pass2, ffmpeg_cmd, get_duration, get_file_stats, get_file_stats_padded,
    get_sample_rate, measure_peak_dbfs,
};
use crate::types::NormResult;

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
    let eq_90   = "equalizer=f=90:width_type=q:width=2.478:g=-2.0";
    let eq_175  = "equalizer=f=175:width_type=q:width=1.0:g=-2.22";
    let eq_360  = "equalizer=f=360:width_type=q:width=1.0:g=-1.23";
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
// New pipeline with Rust DSP (when automixer + new modules are active)
// ---------------------------------------------------------------------------

/// Processes the file using Rust DSP (SG/NN + Voice EQ) between the de-esser and the rest of the chain.
fn process_with_rust_dsp(
    input: &Path,
    output: &Path,
    target_lufs: Option<f32>,
    target_peak_dbfs: f32,
    use_sg: bool,
    use_nn: bool,
    use_dfn3: bool,
    dfn3_mix: f32,
    dfn3_pf: bool,
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

    // Czytaj stderr w osobnym wątku, żeby nie zablokować ffmpega
    // przy dużym wyjściu diagnostycznym.
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

    // Dereverb FIRST — reverb tails would otherwise confuse the gate
    // and smear bands that the EQ would later emphasize.
    if use_dfn3 {
        let params = audio_effects::DereverbParams {
            mix: dfn3_mix,
            attenuation_limit: 30.0,
            post_filter: dfn3_pf,
        };
        // Szukaj deep-filter.exe najpierw obok ffmpeg, a jeśli ffmpeg jest z PATH
        // (brak sensownego parenta), to obok naszego exe.
        let dfn_path = ffmpeg
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("deep-filter.exe"))
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("deep-filter.exe")))
            })
            .ok_or_else(|| anyhow!("cannot locate deep-filter.exe"))?;
        processed = audio_effects::apply_dereverb_dfn3(&processed, &params, &dfn_path, ffmpeg)?;
    }

    // Denoise: spectral gate OR nnnoise (mutually exclusive)
    if use_sg {
        processed = audio_effects::apply_spectral_gate(
            &processed,
            48000,
            1,
            &audio_effects::SpectralGateParams::default(),
        )?;
    } else if use_nn {
        processed = audio_effects::apply_nnnoise(
            &processed,
            &audio_effects::NnnoiseParams::default(),
        )?;
    }

    // Voice EQ always at 50% strength — LAST in the chain (in-place)
    audio_effects::apply_voice_eq_inplace(&mut processed, 48000, 1, 0.5)?;

    // 5. Save processed samples to second temporary file
    let temp_out = NamedTempFile::new()?;
    {
        // BufWriter — jedna alokacja zamiast write_all per próbkę (potencjalnie miliony).
        let mut file = std::io::BufWriter::new(std::fs::File::create(temp_out.path())?);
        // Bezpieczne reinterpretowanie Vec<f32> jako bajtów little-endian:
        for sample in &processed {
            file.write_all(&sample.to_le_bytes())?;
        }
        file.flush()?;
    }

    // 6. Second FFmpeg call: raw → filters (HPF, EQ, compressor) → normalization → encoding
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-f", "f32le", "-ar", "48000", "-ac", "1"])
        .arg("-i")
        .arg(temp_out.path())
        .arg("-vn");

    let post_filters = post_deesser_filters();

    // Normalization and encoding – adapted from existing logic
    let norm_result = if let Some(lufs) = target_lufs {
        match get_file_stats(input, ffmpeg, lufs, Some(&post_filters))? {
            Some(stats) => {
                apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, Some(&post_filters));
                NormResult::Standard
            }
            None => {
                let duration = get_duration(input, ffmpeg).unwrap_or(0.0);
                if duration >= 1.0 {
                    let pad_secs = f32::max(5.0, duration + 1.0);
                    match get_file_stats_padded(
                        input, ffmpeg, lufs, pad_secs, Some(&post_filters),
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
                    match measure_peak_dbfs(input, ffmpeg, Some(&post_filters)) {
                        Ok(peak_dbfs) if peak_dbfs.is_finite() => {
                            let gain_db = target_peak_dbfs - peak_dbfs;
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

    cmd.args(["-c:a", "adpcm_ima_wav"]).arg(output);

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
    target_lufs: Option<f32>,
    target_peak_dbfs: f32,
    automixer: bool,
    automixer_sg: bool,
    automixer_nn: bool,
    automixer_dfn3: bool,
    automixer_dfn3_mix: f32,
    automixer_dfn3_pf: bool,
    ffmpeg: &Path,
) -> Result<NormResult> {
    let rel_path = input.strip_prefix(input_base)?;
    let output = output_base.join(rel_path).with_extension("wav");

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // If automixer is enabled, use the new pipeline with Rust DSP.
    // Otherwise, use old logic (no additional modules).
    if automixer {
        return process_with_rust_dsp(
            input,
            &output,
            target_lufs,
            target_peak_dbfs,
            automixer_sg,
            automixer_nn,
            automixer_dfn3,
            automixer_dfn3_mix,
            automixer_dfn3_pf,
            ffmpeg,
        );
    }

    // Old pipeline (without automixer)
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"]).arg(input).arg("-vn");

    let norm_result = if let Some(lufs) = target_lufs {
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
                            let gain_db = target_peak_dbfs - peak_dbfs;
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

    cmd.args(["-c:a", "adpcm_ima_wav"]).arg(&output);

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
