//! End-to-end / property-style tests for the full `process_single_file`
//! pipeline: real ffmpeg, synthetic fixtures, asserting measurable output
//! properties (format, loudness, finiteness) rather than byte-exact golden
//! files -- lossy encoders and loudnorm's internal EBU R128 implementation
//! aren't guaranteed bit-for-bit stable across ffmpeg versions, so pinning
//! exact bytes here would be brittle across environments.
//!
//! Gated behind `#[ignore]`, same as `tests/ffmpeg_integration.rs`. Run with:
//!
//!   cargo test --test pipeline_e2e -- --ignored

mod common;

use common::{
    ffmpeg_has_encoder, ffmpeg_path, read_wav_samples_f32, skip_if_no_ffmpeg, write_sine_wav,
};
use vocan::processing::process_single_file;
use vocan::types::{OutputFormat, ProcessingOptions, ReductionProfile};

fn base_opts() -> ProcessingOptions {
    ProcessingOptions {
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
        output_format: OutputFormat::Pcm24Wav,
        bitrate_kbps: 128,
    }
}

#[test]
#[ignore]
fn output_matches_requested_format_and_extension() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();

    for &format in OutputFormat::all() {
        let codec = format.ffmpeg_codec();
        if !ffmpeg_has_encoder(codec) {
            eprintln!("SKIP {format:?}: ffmpeg has no '{codec}' encoder in this environment");
            continue;
        }

        let input_path = input_base.join("tone.wav");
        write_sine_wav(&input_path, 3.0, 44100, 440.0, 0.5);

        let mut opts = base_opts();
        opts.output_format = format;

        process_single_file(
            &input_path,
            &input_base,
            &output_base,
            &opts,
            &ffmpeg_path(),
        )
        .unwrap_or_else(|e| panic!("processing failed for {format:?}: {e}"));

        // Mirrors process_single_file's own output-path computation:
        // output_base.join(rel_path).with_extension(format.extension()).
        let output_path = output_base
            .join("tone.wav")
            .with_extension(format.extension());
        assert!(
            output_path.exists(),
            "missing output for {format:?} at {output_path:?}"
        );
    }
}

#[test]
#[ignore]
fn output_lufs_within_tolerance_of_target() {
    if skip_if_no_ffmpeg() {
        return;
    }
    use vocan::ffmpeg::measure_lufs;

    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let input_path = input_base.join("tone.wav");
    write_sine_wav(&input_path, 5.0, 44100, 440.0, 0.5);

    let mut opts = base_opts();
    opts.target_lufs = Some(-16.0);
    opts.output_format = OutputFormat::Pcm24Wav; // lossless -> tight tolerance is fair

    process_single_file(
        &input_path,
        &input_base,
        &output_base,
        &opts,
        &ffmpeg_path(),
    )
    .expect("processing should succeed");

    let output_path = output_base.join("tone.wav");
    let measured = measure_lufs(&output_path, &ffmpeg_path())
        .expect("ffmpeg ok")
        .expect("measurable LUFS on output");
    assert!(
        (measured - (-16.0)).abs() < 0.5,
        "measured {measured} LUFS, expected ~-16.0"
    );
}

#[test]
#[ignore]
fn automixer_pipeline_produces_finite_non_clipping_output() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let input_path = input_base.join("tone.wav");
    write_sine_wav(&input_path, 3.0, 44100, 440.0, 0.4);

    let mut opts = base_opts();
    opts.automixer = true;
    opts.automixer_spectral_gate = true;
    opts.automixer_expander = true;
    opts.output_format = OutputFormat::Pcm24Wav; // easy to read back with hound

    process_single_file(
        &input_path,
        &input_base,
        &output_base,
        &opts,
        &ffmpeg_path(),
    )
    .expect("automixer pipeline should succeed");

    let output_path = output_base.join("tone.wav");
    let samples = read_wav_samples_f32(&output_path);
    assert!(!samples.is_empty(), "output has no samples");
    assert!(
        samples.iter().all(|s| s.is_finite()),
        "output contains NaN/Inf"
    );
    assert!(
        samples.iter().all(|s| s.abs() <= 1.05),
        "output exceeds expected ceiling (clipping?)"
    );
}

/// Regression test for a bug where the Rust-DSP (Automixer) pipeline left
/// the output file at its internal 48kHz processing rate instead of
/// restoring the source's original sample rate, whenever normalization was
/// off or fell back past the Standard/Padded EBU R128 passes (see
/// `apply_norm_decision` in src/processing.rs). Uses a 44.1kHz source with
/// normalization disabled -- exactly the case that previously skipped `-ar`
/// entirely.
#[test]
#[ignore]
fn automixer_without_normalization_preserves_source_sample_rate() {
    if skip_if_no_ffmpeg() {
        return;
    }
    use vocan::ffmpeg::get_sample_rate;

    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let input_path = input_base.join("tone.wav");
    write_sine_wav(&input_path, 2.0, 44100, 440.0, 0.4);

    let mut opts = base_opts();
    opts.target_lufs = None; // normalization off -- previously skipped `-ar` entirely
    opts.automixer = true;
    opts.automixer_spectral_gate = true;
    opts.output_format = OutputFormat::Pcm24Wav;

    process_single_file(
        &input_path,
        &input_base,
        &output_base,
        &opts,
        &ffmpeg_path(),
    )
    .expect("automixer pipeline without normalization should succeed");

    let output_path = output_base.join("tone.wav");
    let sr = get_sample_rate(&output_path, &ffmpeg_path()).expect("output sample rate");
    assert_eq!(
        sr, 44100,
        "expected output resampled back to source rate (44100), got {sr}"
    );
}

/// The Rust DSP stages take their sample rate as a parameter, so the pipeline
/// only forces 48kHz when a stage that genuinely requires it is enabled
/// (nnnoiseless or DeepFilterNet3). With neither on, a 44.1kHz source is
/// processed at 44.1kHz end to end and never round-trips through 48kHz.
///
/// This asserts the observable half of that: the output rate. The absence of
/// the intermediate resample is not visible in the output file itself, which
/// is exactly why the previous test above (which pins the same output rate for
/// a different reason) does not cover it.
#[test]
#[ignore]
fn automixer_at_source_rate_still_produces_correct_output_rate() {
    if skip_if_no_ffmpeg() {
        return;
    }
    use vocan::ffmpeg::get_sample_rate;

    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();

    // 22050 Hz: neither the source rate nor the old hard-coded DSP rate, so a
    // stray 48000 anywhere in the chain would show up plainly.
    for &(rate, nn) in &[(22050u32, false), (44100, false), (44100, true)] {
        let input_path = input_base.join(format!("tone_{rate}_{nn}.wav"));
        write_sine_wav(&input_path, 2.0, rate, 440.0, 0.4);

        let mut opts = base_opts();
        opts.automixer = true;
        opts.automixer_expander = true;
        // With nn = true the DSP must internally run at 48kHz (RNNoise
        // requires it) and still restore `rate` on the way out.
        opts.automixer_nn_dereverb = nn;
        opts.output_format = OutputFormat::Pcm24Wav;

        process_single_file(
            &input_path,
            &input_base,
            &output_base,
            &opts,
            &ffmpeg_path(),
        )
        .unwrap_or_else(|e| panic!("rate={rate} nn={nn} failed: {e}"));

        let output_path = output_base
            .join(format!("tone_{rate}_{nn}.wav"))
            .with_extension("wav");
        let sr = get_sample_rate(&output_path, &ffmpeg_path()).expect("output sample rate");
        assert_eq!(sr, rate, "rate={rate} nn={nn}: got output at {sr} Hz");
    }
}

#[test]
#[ignore]
fn automixer_toggle_combinations_do_not_error() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();

    // (spectral_gate, nn_dereverb, expander). DFN3 dereverb is intentionally
    // excluded from this combination matrix -- it shells out to an external
    // `deep-filter` binary/model that a CI runner won't have installed, and
    // this matrix is meant to run everywhere ffmpeg does. DFN3 has its own
    // dedicated, separately-gated tests in tests/dfn3_integration.rs. Spectral
    // gate and nnnoise are mutually exclusive in the app's own UI;
    // process_with_rust_dsp just checks spectral_gate first, so that pairing
    // is mirrored here.
    let combos: &[(bool, bool, bool)] = &[
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, false, true),
        (false, true, true),
    ];

    for (i, &(sg, nn, exp)) in combos.iter().enumerate() {
        let input_path = input_base.join(format!("tone_{i}.wav"));
        write_sine_wav(&input_path, 2.0, 48000, 300.0, 0.4);

        let mut opts = base_opts();
        opts.automixer = true;
        opts.automixer_spectral_gate = sg;
        opts.automixer_nn_dereverb = nn;
        opts.automixer_expander = exp;
        opts.output_format = OutputFormat::Pcm24Wav;

        process_single_file(
            &input_path,
            &input_base,
            &output_base,
            &opts,
            &ffmpeg_path(),
        )
        .unwrap_or_else(|e| panic!("combo sg={sg} nn={nn} exp={exp} failed: {e}"));
    }
}
