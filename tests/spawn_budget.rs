//! Pins the number of ffmpeg processes VOCAN launches per file.
//!
//! For the short lines this tool exists to batch, process startup dominates:
//! converting a two-second take costs less than launching the processes that do
//! it. So the per-file process count is a performance budget, and one that is
//! easy to blow by accident -- reaching for a probe in a new place, or splitting
//! one measurement pass into two, costs a process on *every* file in a batch
//! without changing a single visible behaviour.
//!
//! This exists because a plain timing test would not catch that. An earlier
//! draft of the memory gate sited it in the batch scanner, where it would have
//! needed its own probe while `process_with_rust_dsp` kept its own -- five
//! processes per file instead of four, with the gate itself never engaging on
//! ordinary input and every throughput measurement looking fine.
//!
//! Run with:
//!
//!   cargo test --test spawn_budget -- --ignored

mod common;

use common::{ffmpeg_path, skip_if_no_ffmpeg, write_sine_wav};
use vocan::ffmpeg::{reset_spawn_count, spawn_count};
use vocan::processing::process_single_file;
use vocan::types::{OutputFormat, ProcessingOptions};

/// Runs one file through the pipeline and returns how many ffmpeg processes it
/// took.
///
/// `process_single_file` is called directly, on this thread and without rayon,
/// so every `ffmpeg_cmd` lands on the thread-local counter being read here.
fn spawns_for(opts: ProcessingOptions, duration_secs: f32) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let input_path = input_base.join("tone.wav");
    // Comfortably past the 3.0s threshold, so the standard EBU R128 pass
    // succeeds and no padded/peak fallback runs -- those are extra processes by
    // design, and this measures the happy path.
    write_sine_wav(&input_path, duration_secs, 44100, 440.0, 0.5);

    reset_spawn_count();
    process_single_file(
        &input_path,
        &input_base,
        &output_base,
        &opts,
        &ffmpeg_path(),
    )
    .expect("processing should succeed");
    spawn_count()
}

#[test]
#[ignore]
fn plain_conversion_costs_three_ffmpeg_processes() {
    if skip_if_no_ffmpeg() {
        return;
    }
    // 1. probe_input (sample rate + duration, one process for both)
    // 2. loudnorm pass 1
    // 3. encode
    let opts = ProcessingOptions {
        target_lufs: Some(-16.0),
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(opts, 5.0), 3);
}

#[test]
#[ignore]
fn automixer_costs_four_ffmpeg_processes() {
    if skip_if_no_ffmpeg() {
        return;
    }
    // 1. probe_input -- also feeds the memory gate's size estimate, which is
    //    precisely why the gate belongs inside process_with_rust_dsp and not in
    //    the batch scanner: sited there it would have needed a fifth process.
    // 2. de-esser pass, streaming f32 samples into Rust
    // 3. loudnorm pass 1 over the processed temp WAV
    // 4. encode
    let opts = ProcessingOptions {
        target_lufs: Some(-16.0),
        automixer: true,
        automixer_spectral_gate: true,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(opts, 5.0), 4);
}

#[test]
#[ignore]
fn skipping_normalization_removes_the_measurement_pass_only() {
    if skip_if_no_ffmpeg() {
        return;
    }
    // Without normalization there is no pass-1 measurement, and the
    // non-Automixer path does not probe either: it has nothing to restore.
    let plain = ProcessingOptions {
        target_lufs: None,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(plain, 5.0), 1, "encode only");

    // The Automixer path still probes (it has to restore the source rate) and
    // still runs the de-esser pass, so it drops from 4 to 3.
    let automixer = ProcessingOptions {
        target_lufs: None,
        automixer: true,
        automixer_spectral_gate: true,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(automixer, 5.0), 3);
}

#[test]
#[ignore]
fn trimming_silence_costs_no_extra_process_in_either_pipeline() {
    if skip_if_no_ffmpeg() {
        return;
    }
    // The design of the whole option, asserted. `silenceremove` is spliced into
    // a filter chain that was going to run anyway -- the encode's own `-af` in
    // the plain path, the de-esser pass in the Automixer path -- so the counts
    // here are the untrimmed counts, unchanged.
    //
    // Giving the trim a pass of its own is the easiest way to write this
    // feature and the reason this test exists: it would cost one extra process
    // and one extra full read of the file, per file, on every batch, and
    // nothing else in the suite would notice.
    let plain = ProcessingOptions {
        target_lufs: Some(-16.0),
        trim_silence: true,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(plain, 5.0), 3, "plain conversion with trim");

    let automixer = ProcessingOptions {
        target_lufs: Some(-16.0),
        automixer: true,
        automixer_spectral_gate: true,
        trim_silence: true,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(automixer, 5.0), 4, "automixer with trim");
}

#[test]
#[ignore]
fn trimming_silence_without_normalization_still_costs_one_process() {
    if skip_if_no_ffmpeg() {
        return;
    }
    // The trim is the only thing in `-af` here, with no normalization pass to
    // hang it off -- the case where a separate trim pass would have been the
    // path of least resistance.
    let opts = ProcessingOptions {
        target_lufs: None,
        trim_silence: true,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    };
    assert_eq!(spawns_for(opts, 5.0), 1, "encode only");
}
