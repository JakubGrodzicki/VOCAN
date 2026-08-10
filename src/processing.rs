use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::audio_effects;
use crate::ffmpeg::{
    apply_loudnorm_pass2, ffmpeg_cmd, get_file_stats, get_file_stats_padded, measure_peak_dbfs,
    probe_input,
};
use crate::memory;
use crate::proc::{self, output_supervised};
use crate::types::{AppMsg, LoudnormStats, NormResult, OutputFormat, ProcessingOptions};

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
    log: Option<&std::sync::mpsc::Sender<AppMsg>>,
) -> NormDecision {
    /// A measurement that fails is not a file that fails.
    ///
    /// There are two more strategies behind each of these passes (padded
    /// loudnorm, then peak normalization), and the peak pass was already
    /// softened with `.ok()`. The EBU R128 passes were not: an unparseable
    /// pass-1 output -- FFmpeg refusing the file, or a build whose loudnorm
    /// prints something unexpected -- propagated straight out with `?` and
    /// killed the file, even though the very next strategy would have produced
    /// a perfectly good result. Note this cannot mask a missing FFmpeg: the
    /// final encoding pass still reports that, with its own stderr attached.
    fn soften(
        result: Result<Option<LoudnormStats>>,
        pass: &str,
        log: Option<&std::sync::mpsc::Sender<AppMsg>>,
    ) -> Option<LoudnormStats> {
        match result {
            Ok(stats) => stats,
            Err(e) => {
                if let Some(tx) = log {
                    let _ = tx.send(AppMsg::Log(format!(
                        "{pass} loudness measurement unavailable ({e}); \
                         falling back to the next strategy."
                    )));
                }
                None
            }
        }
    }

    let standard_stats = if duration >= 3.0 {
        soften(
            get_file_stats(input, ffmpeg, target_lufs, prefix),
            "Standard",
            log,
        )
    } else {
        None
    };
    // `decide_normalization` stays the single source of truth for the
    // precedence rules; taking the stats back out of the decision (rather than
    // unwrapping the Option alongside it) keeps that from depending on an
    // unstated invariant about when Standard can be returned.
    if let NormDecision::Standard(stats) = decide_normalization(
        duration,
        standard_stats.as_ref(),
        None,
        None,
        target_peak_dbfs,
    ) {
        return NormDecision::Standard(stats);
    }

    let pad_secs = f32::max(5.0, duration + 1.0);
    let padded_stats = soften(
        get_file_stats_padded(input, ffmpeg, target_lufs, pad_secs, prefix),
        "Padded",
        log,
    );
    if let NormDecision::Padded(stats) = decide_normalization(
        duration,
        standard_stats.as_ref(),
        padded_stats.as_ref(),
        None,
        target_peak_dbfs,
    ) {
        return NormDecision::Padded(stats);
    }

    let peak_dbfs = measure_peak_dbfs(input, ffmpeg, prefix)
        .ok()
        .filter(|p| p.is_finite());
    decide_normalization(
        duration,
        standard_stats.as_ref(),
        padded_stats.as_ref(),
        peak_dbfs,
        target_peak_dbfs,
    )
}

/// Applies a [`NormDecision`] to the FFmpeg command (pass-2 filter args) and
/// returns the corresponding [`NormResult`] for logging.
///
/// Every branch explicitly sets `-ar source_sr`, even when the caller's own
/// pipeline never resampled the signal (in which case this is a no-op, since
/// FFmpeg would already output at `source_sr`). This matters for the Rust-DSP
/// (Automixer) pipeline, which always works at a fixed 48kHz internally: the
/// Peak/Skipped fallbacks used to omit `-ar` entirely, silently leaving the
/// exported file at 48kHz instead of the original sample rate whenever
/// loudness normalization was off or fell back past the Standard/Padded
/// EBU R128 passes.
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
            cmd.args(["-af", &filter, "-ar", &source_sr.to_string()]);
            NormResult::Peak { gain_db: *gain_db }
        }
        NormDecision::Skipped => {
            if let Some(p) = prefix {
                cmd.args(["-af", p]);
            }
            cmd.args(["-ar", &source_sr.to_string()]);
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

/// The scratch name the encoder writes to before the result is committed.
///
/// The extension is preserved (`line1.wav` -> `line1.vocan-partial.wav`)
/// because FFmpeg picks its muxer from it.
fn partial_path(output: &Path) -> std::path::PathBuf {
    let mut name = output.file_stem().unwrap_or_default().to_os_string();
    name.push(".vocan-partial.");
    name.push(output.extension().unwrap_or_default());
    output.with_file_name(name)
}

/// Runs the final encoding pass, publishing its result only if it completed.
///
/// FFmpeg writes straight to the path it is given, so an interrupted run leaves
/// a truncated file sitting at the destination looking exactly like a finished
/// conversion -- and a half-written voice-over line is worse than a missing one,
/// because nothing downstream can tell. That was survivable while Stop only took
/// effect between files; now that it terminates children mid-encode (and closing
/// the window does too), it is a live hazard.
///
/// So: encode to a sibling scratch name and rename on success. Rename within one
/// directory is atomic on both Windows and POSIX, and `std::fs::rename` replaces
/// an existing destination on both.
fn run_encode(cmd: &mut Command, output: &Path) -> Result<()> {
    let partial = partial_path(output);
    cmd.arg(&partial);

    let result = output_supervised(cmd).context("Failed to run FFmpeg (Conversion)");
    let cleanup = || {
        let _ = std::fs::remove_file(&partial);
    };

    let final_output = match result {
        Ok(out) => out,
        Err(e) => {
            cleanup();
            return Err(e);
        }
    };
    if !final_output.status.success() {
        cleanup();
        return Err(crate::ffmpeg::ffmpeg_failed(
            "conversion",
            &String::from_utf8_lossy(&final_output.stderr),
        ));
    }

    std::fs::rename(&partial, output).map_err(|e| {
        cleanup();
        anyhow!(
            "encoded {} but could not move it into place: {}",
            partial.display(),
            e
        )
    })
}

/// Reads a raw little-endian f32 stream directly into a `Vec<f32>`.
///
/// Buffering the whole stream into a `Vec<u8>` and converting afterwards would
/// keep a second full-length copy of the signal alive (115 MB for a 10-minute
/// mono 48kHz file) for the remainder of the pipeline. `Read::read` makes no
/// promise about landing on a 4-byte boundary, so a 0-3 byte remainder is
/// carried across iterations.
fn read_f32le_stream(reader: &mut impl Read) -> Result<Vec<f32>> {
    /// `Vec::try_reserve` with a message a user can act on.
    ///
    /// This is the one allocation in the pipeline whose size is not known in
    /// advance, so it is the one that has to grow fallibly: an infallible
    /// `extend` that cannot get memory **aborts the process**, which
    /// `catch_unwind` in the batch worker never sees -- one oversized file
    /// would take the whole run down rather than being skipped with an error.
    ///
    /// `try_reserve` (not `try_reserve_exact`) because it keeps `Vec`'s
    /// geometric growth; reserving exactly per chunk would make this O(n^2).
    fn grow(samples: &mut Vec<f32>, extra: usize) -> Result<()> {
        samples.try_reserve(extra).map_err(|_| {
            anyhow!(
                "ran out of memory decoding this file ({:.2} GB of samples read so far). \
                 Process it without Automixer, or split it into shorter takes.",
                (samples.len() as f64 * 4.0) / (1024.0 * 1024.0 * 1024.0)
            )
        })
    }

    let mut samples: Vec<f32> = Vec::new();
    grow(&mut samples, 1 << 18)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut rem = [0u8; 4];
    let mut rem_len = 0usize;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut chunk = &buf[..n];

        // Finish the sample straddling the previous read, if any.
        if rem_len > 0 {
            let take = (4 - rem_len).min(chunk.len());
            rem[rem_len..rem_len + take].copy_from_slice(&chunk[..take]);
            rem_len += take;
            chunk = &chunk[take..];
            if rem_len == 4 {
                grow(&mut samples, 1)?;
                samples.push(f32::from_le_bytes(rem));
                rem_len = 0;
            }
        }

        let usable = chunk.len() - chunk.len() % 4;
        grow(&mut samples, usable / 4)?;
        samples.extend(
            chunk[..usable]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );

        // Only overwrite the remainder when this read actually left one --
        // `chunk` can be empty here after topping up a previous remainder.
        let tail = &chunk[usable..];
        if !tail.is_empty() {
            rem[..tail.len()].copy_from_slice(tail);
            rem_len = tail.len();
        }
    }

    Ok(samples)
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
    // 1. Get original sample rate (to restore later).
    //
    // `probe_input` rather than `get_sample_rate`: both spawn exactly one
    // ffmpeg process and parse the same stderr, but this one also returns the
    // duration, which the memory gate below needs to size its request. The
    // per-file process count is unchanged.
    let (probed_sr, probed_duration) = probe_input(input, ffmpeg);
    let source_sr = probed_sr.unwrap_or(44100);

    // Only nnnoiseless (RNNoise) and DeepFilterNet3 hard-require 48kHz; every
    // other stage takes the rate as a parameter and is correct at any of them.
    // When neither is enabled we run the DSP at the source rate, which drops
    // two sample-rate conversions per file (source -> 48k on decode, 48k ->
    // source on encode) along with the quality loss they carry.
    let needs_48k = opts.automixer_dfn3_dereverb || opts.automixer_nn_dereverb;
    let dsp_sr: u32 = if needs_48k { 48_000 } else { source_sr };

    // Admission gate for the in-memory stages below. Weighted by this file's
    // own estimated footprint, so a batch of short voice-over lines never
    // contends and pays only one uncontended mutex acquisition per file. It
    // exists for the collective case -- `cores - 1` files that each fit but
    // together do not -- where the alternative is a failed allocation, which in
    // Rust aborts the process and loses the entire run.
    let memory_permit = memory::gate().acquire(
        memory::estimated_dsp_bytes(probed_duration, dsp_sr, opts.automixer_dfn3_dereverb),
        |msg| {
            if let Some(tx) = &opts.log {
                let _ = tx.send(AppMsg::Log(msg));
            }
        },
    );

    // 2+3. FFmpeg de-esser → stdout (f32le) → memory (no temp file)
    let mut child = ffmpeg_cmd(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &deesser_only_filter()])
        .args(["-ac", "1", "-ar", &dsp_sr.to_string()])
        .args(["-f", "f32le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("FFmpeg spawn failed (de-esser pass)")?;

    // Registered so Stop / closing the window can terminate this pass mid-file
    // instead of only between files. Declared after `child` so it drops first;
    // see `proc::register` for why that ordering is load-bearing.
    let _child_guard = proc::register(&child);

    // Read stderr in a separate thread to avoid blocking FFmpeg
    // when there is large diagnostic output.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    // `Child::drop` neither kills nor reaps. Propagating a read error with `?`
    // straight out of this scope therefore used to leak a live ffmpeg process
    // *and* the stderr thread above, permanently blocked in `read_to_string` --
    // once per failing file, for the rest of the session. So: take the result,
    // tear the child down unconditionally, and only then propagate.
    let read_result = match child.stdout.as_mut() {
        Some(stdout) => read_f32le_stream(stdout),
        None => Err(anyhow!("FFmpeg stdout not piped")),
    };
    // Close our read end before waiting. If the read above bailed out early,
    // ffmpeg is still producing output, and leaving the pipe open would block it
    // as soon as the buffer filled -- deadlocking the `wait()` below.
    drop(child.stdout.take());
    let status = child.wait();
    let stderr_text = stderr_handle.join().unwrap_or_default();

    let samples = read_result.context("reading FFmpeg de-esser output")?;
    let status = status.context("FFmpeg de-esser wait failed")?;
    if !status.success() {
        return Err(anyhow!("FFmpeg de-esser pass failed: {}", stderr_text));
    }

    // 4. Rust DSP
    let mut processed = samples;

    // --- Noise floor estimation (BEFORE dereverb) ---
    // The spec requires analysis on the raw/pre-dereverb signal — denoising
    // distorts the floor characteristic and biases the estimate.
    // We run it on the post-de-esser samples (the earliest point in Rust memory);
    // the de-esser is a subtle HF filter that won't materially affect broadband
    // RMS noise-floor estimation.
    let expander_noise_floor = if opts.automixer_expander {
        audio_effects::estimate_noise_floor_db(&processed, dsp_sr)
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
            audio_effects::apply_expander_inplace(
                &mut processed,
                dsp_sr,
                1,
                &params,
                noise_floor_db,
            );
        }
    }

    // Denoise: spectral gate OR nnnoise (mutually exclusive)
    if opts.automixer_spectral_gate {
        processed = audio_effects::apply_spectral_gate(
            &processed,
            dsp_sr,
            1,
            &audio_effects::SpectralGateParams::default(),
        )?;
    } else if opts.automixer_nn_dereverb {
        processed =
            audio_effects::apply_nnnoise(&processed, &audio_effects::NnnoiseParams::default())?;
    }

    // Voice EQ always at 50% strength — LAST in the chain (in-place)
    audio_effects::apply_voice_eq_inplace(&mut processed, dsp_sr, 1, 0.5)?;

    // 5. Write processed samples to a temp WAV file using hound.
    //    This file is used for BOTH loudnorm pass-1 measurement AND as the
    //    input to the final FFmpeg encoding pass, ensuring the signal measured
    //    is the same signal that gets normalized and encoded.
    let mut temp_builder = tempfile::Builder::new();
    temp_builder.suffix(".wav");
    let temp_wav = match proc::scratch_dir() {
        Some(dir) => temp_builder.tempfile_in(dir),
        None => temp_builder.tempfile(),
    }
    .context("cannot create temp wav for DSP output")?;
    let temp_wav_path = temp_wav.path().to_path_buf();
    let processed_len = processed.len();
    {
        let spec = hound::WavSpec {
            sample_rate: dsp_sr,
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
    // The signal now lives on disk and is only ever read back through FFmpeg;
    // holding the in-memory copy through the measurement and encoding passes
    // below would pin a full-length buffer for no reason.
    drop(processed);
    // The signal lives on disk from here on and the remaining passes stream it
    // through ffmpeg, so the reservation goes back to the pool now rather than
    // at the end of the function.
    drop(memory_permit);

    // 6. Second FFmpeg call: temp WAV → filters (HPF, EQ, compressor) → normalization → encoding
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"])
        .arg(&temp_wav_path)
        .arg("-vn");

    let post_filters = post_deesser_filters();

    // Normalization and encoding — stats measured on the PROCESSED temp WAV.
    let norm_result = if let Some(lufs) = opts.target_lufs {
        // We wrote this WAV ourselves one step above, so its duration is known
        // exactly -- shelling out to FFmpeg just to re-measure it would be a
        // whole extra process per file.
        let duration = processed_len as f32 / dsp_sr as f32;
        let decision = measure_and_decide_normalization(
            &temp_wav_path,
            ffmpeg,
            lufs,
            duration,
            Some(&post_filters),
            opts.target_peak_dbfs,
            opts.log.as_ref(),
        );
        apply_norm_decision(&mut cmd, &decision, lufs, source_sr, Some(&post_filters))
    } else {
        // No normalization requested at all -- still must restore the
        // original sample rate, since the DSP stage above always runs at a
        // fixed 48kHz and the temp WAV we're now reading is 48kHz too.
        cmd.args(["-af", &post_filters, "-ar", &source_sr.to_string()]);
        NormResult::Skipped
    };

    add_format_args(&mut cmd, opts.output_format, opts.bitrate_kbps);
    run_encode(&mut cmd, output)?;

    Ok(norm_result)
}

// ---------------------------------------------------------------------------
// Main file processing function
// ---------------------------------------------------------------------------

/// Where the processed version of `input` will be written.
///
/// Exposed so the batch scanner can detect two inputs that map to the same
/// output *before* processing starts, rather than letting two ffmpeg processes
/// write the same file at once. Both callers going through one function is what
/// makes that check agree with reality by construction.
pub fn output_path_for(
    input: &Path,
    input_base: &Path,
    output_base: &Path,
    format: OutputFormat,
) -> Result<std::path::PathBuf> {
    let rel_path = input.strip_prefix(input_base)?;
    Ok(output_base
        .join(rel_path)
        .with_extension(format.extension()))
}

pub fn process_single_file(
    input: &Path,
    input_base: &Path,
    output_base: &Path,
    opts: &ProcessingOptions,
    ffmpeg: &Path,
) -> Result<NormResult> {
    let output = output_path_for(input, input_base, output_base, opts.output_format)?;

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
        // One probe process yields both values; they come from the same stderr.
        let (probed_sr, probed_duration) = probe_input(input, ffmpeg);
        let source_sr = probed_sr.unwrap_or(44100);
        let duration = probed_duration.unwrap_or(0.0);
        let decision = measure_and_decide_normalization(
            input,
            ffmpeg,
            lufs,
            duration,
            None,
            opts.target_peak_dbfs,
            opts.log.as_ref(),
        );
        apply_norm_decision(&mut cmd, &decision, lufs, source_sr, None)
    } else {
        NormResult::Skipped
    };

    add_format_args(&mut cmd, opts.output_format, opts.bitrate_kbps);
    run_encode(&mut cmd, &output)?;

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
        assert_eq!(
            args,
            vec!["-af", "highpass=f=70,volume=2.5000dB", "-ar", "44100"]
        );
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
        assert_eq!(
            args_of(&cmd),
            vec!["-af", "volume=-1.0000dB", "-ar", "44100"]
        );
    }

    #[test]
    fn apply_peak_decision_restores_source_sample_rate_even_when_it_differs_from_44100() {
        // Regression test: the Rust-DSP (Automixer) pipeline always measures
        // and encodes at 48kHz internally, so `apply_norm_decision` must
        // restore whatever `source_sr` the *original* input actually had --
        // previously this branch never set `-ar` at all, silently leaving
        // Automixer output stuck at 48kHz whenever normalization fell back
        // to Peak.
        let mut cmd = Command::new("ffmpeg");
        apply_norm_decision(
            &mut cmd,
            &NormDecision::Peak { gain_db: 1.0 },
            -16.0,
            22050,
            None,
        );
        assert_eq!(
            args_of(&cmd),
            vec!["-af", "volume=1.0000dB", "-ar", "22050"]
        );
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
        assert_eq!(args_of(&cmd), vec!["-af", "highpass=f=70", "-ar", "44100"]);
    }

    // -----------------------------------------------------------------------
    // output_path_for
    // -----------------------------------------------------------------------

    #[test]
    fn output_path_for_mirrors_the_input_tree_and_rewrites_the_extension() {
        let out = output_path_for(
            Path::new("/in/voice/line1.mp3"),
            Path::new("/in"),
            Path::new("/out"),
            OutputFormat::Flac,
        )
        .unwrap();
        assert_eq!(out, Path::new("/out/voice/line1.flac"));
    }

    #[test]
    fn output_path_for_maps_different_source_extensions_onto_one_output() {
        // The collision the batch scanner has to catch: because every format
        // rewrites the extension, two sources that differ only in theirs land
        // on the same path -- and the batch processes files in parallel, so
        // both would be written at once.
        let a = output_path_for(
            Path::new("/in/line1.wav"),
            Path::new("/in"),
            Path::new("/out"),
            OutputFormat::AdpcmWav,
        )
        .unwrap();
        let b = output_path_for(
            Path::new("/in/line1.mp3"),
            Path::new("/in"),
            Path::new("/out"),
            OutputFormat::AdpcmWav,
        )
        .unwrap();
        assert_eq!(a, b, "this is the collision, and it must be detectable");
    }

    #[test]
    fn partial_path_keeps_the_extension_ffmpeg_selects_its_muxer_from() {
        assert_eq!(
            partial_path(Path::new("/out/voice/line1.wav")),
            Path::new("/out/voice/line1.vocan-partial.wav")
        );
        assert_eq!(
            partial_path(Path::new("/out/line1.flac")),
            Path::new("/out/line1.vocan-partial.flac")
        );
    }

    #[test]
    fn partial_path_stays_in_the_destination_directory() {
        // The commit is a rename, which is only atomic within one filesystem
        // directory -- so the scratch file must be a sibling of the target.
        let out = Path::new("/out/voice/line1.wav");
        assert_eq!(partial_path(out).parent(), out.parent());
    }

    #[test]
    fn output_path_for_errors_when_the_input_is_outside_the_base() {
        assert!(output_path_for(
            Path::new("/elsewhere/line1.wav"),
            Path::new("/in"),
            Path::new("/out"),
            OutputFormat::AdpcmWav,
        )
        .is_err());
    }

    #[test]
    fn apply_skipped_decision_without_prefix_still_restores_sample_rate() {
        // Regression test: previously this branch added no args at all,
        // which meant the Automixer pipeline's Skipped fallback (e.g. a
        // fully silent file) left the output at the DSP stage's fixed
        // 48kHz instead of the original source rate.
        let mut cmd = Command::new("ffmpeg");
        apply_norm_decision(&mut cmd, &NormDecision::Skipped, -16.0, 48000, None);
        assert_eq!(args_of(&cmd), vec!["-ar", "48000"]);
    }
}
