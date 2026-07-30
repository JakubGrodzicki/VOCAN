//! Integration tests that shell out to a real `ffmpeg` binary.
//!
//! Gated behind `#[ignore]` so a plain `cargo test` stays fast and green on
//! any machine, ffmpeg or not. Run these explicitly with:
//!
//!   cargo test -- --ignored
//!
//! Each test also runs a runtime `ffmpeg_available()` check and skips
//! (rather than panics) if ffmpeg isn't on PATH, as a second safety net.

mod common;

use common::{ffmpeg_path, skip_if_no_ffmpeg, write_sine_wav};
use vocan::ffmpeg::{
    get_duration, get_file_stats, get_file_stats_padded, get_sample_rate, measure_peak_dbfs,
};

#[test]
#[ignore]
fn get_duration_reports_correct_seconds_within_tolerance() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.wav");
    write_sine_wav(&path, 2.345, 44100, 440.0, 0.5);

    let duration = get_duration(&path, &ffmpeg_path()).expect("duration");
    assert!((duration - 2.345).abs() < 0.05, "got {duration}");
}

#[test]
#[ignore]
fn get_sample_rate_matches_generated_wav() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for &sr in &[44100u32, 48000u32] {
        let path = dir.path().join(format!("tone_{sr}.wav"));
        write_sine_wav(&path, 1.0, sr, 440.0, 0.5);

        let got = get_sample_rate(&path, &ffmpeg_path()).expect("sample rate");
        assert_eq!(got, sr);
    }
}

#[test]
#[ignore]
fn get_file_stats_standard_pass_succeeds_at_5s() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone5s.wav");
    write_sine_wav(&path, 5.0, 44100, 440.0, 0.5);

    let stats = get_file_stats(&path, &ffmpeg_path(), -16.0, None).expect("ffmpeg ok");
    assert!(stats.is_some(), "expected Some(stats) for a 5s tone");
}

#[test]
#[ignore]
fn get_file_stats_padded_succeeds_on_short_clip() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short1_5s.wav");
    write_sine_wav(&path, 1.5, 44100, 440.0, 0.5);

    let stats = get_file_stats_padded(&path, &ffmpeg_path(), -16.0, 5.0, None).expect("ffmpeg ok");
    assert!(
        stats.is_some(),
        "expected Some(stats) with padding on a 1.5s tone"
    );
}

#[test]
#[ignore]
fn measure_peak_dbfs_matches_known_amplitude() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peak_test.wav");
    // amplitude 0.5 => peak dBFS = 20*log10(0.5) = -6.02 dBFS
    write_sine_wav(&path, 2.0, 44100, 1000.0, 0.5);

    let peak = measure_peak_dbfs(&path, &ffmpeg_path(), None).expect("ffmpeg ok");
    assert!((peak - (-6.02)).abs() < 0.5, "got {peak}");
}

/// This does NOT require ffmpeg to be installed -- it exercises the error
/// path when the ffmpeg binary can't be found/spawned, so it always runs
/// (not `#[ignore]`d) as a regression guard for graceful failure.
#[test]
fn process_single_file_with_missing_ffmpeg_returns_err_not_panic() {
    use std::path::PathBuf;
    use vocan::processing::process_single_file;
    use vocan::types::{OutputFormat, ProcessingOptions, ReductionProfile};

    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let input_path = input_base.join("tone.wav");
    write_sine_wav(&input_path, 1.0, 44100, 440.0, 0.5);

    let opts = ProcessingOptions {
        target_lufs: None,
        target_peak_dbfs: -3.0,
        automixer: false,
        automixer_spectral_gate: false,
        automixer_nn_dereverb: false,
        automixer_dfn3_dereverb: false,
        automixer_dfn3_mix: 0.8,
        automixer_dfn3_postfilter: false,
        automixer_expander: false,
        automixer_expander_safety_pct: 50.0,
        automixer_expander_reduction_profile: ReductionProfile::Recommended,
        output_format: OutputFormat::Pcm32fWav,
        bitrate_kbps: 128,
    };

    let bogus_ffmpeg = PathBuf::from("/nonexistent/ffmpeg-binary-xyz");
    let result = process_single_file(&input_path, &input_base, &output_base, &opts, &bogus_ffmpeg);
    assert!(result.is_err(), "expected Err, got {result:?}");
}

/// Runs the real (post-refactor) `process_single_file` pipeline at each
/// boundary duration from the `decide_normalization` unit-test table in
/// `src/processing.rs`, confirming the pure-logic table matches real ffmpeg
/// behavior end-to-end, not just the mocked-input table.
#[test]
#[ignore]
fn norm_result_matches_decision_table_at_boundary_durations() {
    if skip_if_no_ffmpeg() {
        return;
    }
    use vocan::processing::process_single_file;
    use vocan::types::{NormResult, OutputFormat, ProcessingOptions, ReductionProfile};

    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    std::fs::create_dir_all(&output_base).unwrap();

    let opts = ProcessingOptions {
        target_lufs: Some(-16.0),
        target_peak_dbfs: -3.0,
        automixer: false,
        automixer_spectral_gate: false,
        automixer_nn_dereverb: false,
        automixer_dfn3_dereverb: false,
        automixer_dfn3_mix: 0.8,
        automixer_dfn3_postfilter: false,
        automixer_expander: false,
        automixer_expander_safety_pct: 50.0,
        automixer_expander_reduction_profile: ReductionProfile::Recommended,
        output_format: OutputFormat::Pcm32fWav,
        bitrate_kbps: 128,
    };

    // Matches the boundary table in src/processing.rs's decide_normalization tests.
    let cases: &[(f32, &str)] = &[
        (0.9, "Padded"),
        (2.9, "Padded"),
        (3.0, "Standard"),
        (3.1, "Standard"),
        (5.0, "Standard"),
    ];

    for (duration, expected) in cases {
        let name = format!("tone_{duration}.wav");
        let input_path = input_base.join(&name);
        write_sine_wav(&input_path, *duration, 44100, 440.0, 0.5);

        let result = process_single_file(
            &input_path,
            &input_base,
            &output_base,
            &opts,
            &ffmpeg_path(),
        )
        .unwrap_or_else(|e| panic!("processing failed for duration={duration}: {e}"));

        let actual = match result {
            NormResult::Standard => "Standard",
            NormResult::Padded => "Padded",
            NormResult::Peak { .. } => "Peak",
            NormResult::Skipped => "Skipped",
        };
        assert_eq!(actual, *expected, "duration={duration}");
    }
}
