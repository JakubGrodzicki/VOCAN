//! Tests for the optional DeepFilterNet3 (DFN3) dereverb integration.
//!
//! Gated on a real `deep-filter` binary actually being available next to
//! `ffmpeg` on PATH (see `common::deep_filter_next_to_ffmpeg`). Unlike the
//! plain ffmpeg-dependent tests, these are NOT expected to run in ordinary
//! CI -- DeepFilterNet3 needs a fairly large bundled model and isn't
//! installed there. Run these on a machine that has both ffmpeg and
//! deep-filter set up (e.g. after `installMacLinux.sh` / `installWindows.ps1`
//! without `--no-dfn3` / `-NoDfn3`):
//!
//!   cargo test --test dfn3_integration -- --ignored

mod common;

use common::{deep_filter_next_to_ffmpeg, write_sine_wav};

#[test]
#[ignore]
fn apply_dereverb_dfn3_produces_finite_output_of_same_length() {
    let Some((ffmpeg, dfn_bin)) = deep_filter_next_to_ffmpeg() else {
        eprintln!("SKIP: deep-filter not found next to ffmpeg on PATH");
        return;
    };
    use vocan::audio_effects::{apply_dereverb_dfn3, DereverbParams};

    // deep-filter expects 48kHz mono input.
    let sample_rate = 48000u32;
    let n = sample_rate as usize; // 1 second
    let samples: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            0.3 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
        })
        .collect();

    let params = DereverbParams::default();
    let out = apply_dereverb_dfn3(&samples, &params, &dfn_bin, &ffmpeg)
        .expect("deep-filter invocation should succeed");

    assert_eq!(
        out.len(),
        samples.len(),
        "dry/wet mix must preserve the sample count"
    );
    assert!(out.iter().all(|s| s.is_finite()), "output contains NaN/Inf");
}

#[test]
#[ignore]
fn apply_dereverb_dfn3_mix_zero_is_close_to_dry_signal() {
    let Some((ffmpeg, dfn_bin)) = deep_filter_next_to_ffmpeg() else {
        eprintln!("SKIP: deep-filter not found next to ffmpeg on PATH");
        return;
    };
    use vocan::audio_effects::{apply_dereverb_dfn3, DereverbParams};

    let sample_rate = 48000u32;
    let n = sample_rate as usize;
    let samples: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            0.3 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
        })
        .collect();

    let params = DereverbParams {
        mix: 0.0,
        ..DereverbParams::default()
    };
    let out = apply_dereverb_dfn3(&samples, &params, &dfn_bin, &ffmpeg).expect("deep-filter ok");

    // mix=0.0 means "dry only" at every sample, regardless of what the model produced.
    for (a, b) in samples.iter().zip(out.iter()) {
        assert!(
            (a - b).abs() < 1e-6,
            "mix=0.0 should reproduce the dry signal exactly"
        );
    }
}

#[test]
#[ignore]
fn process_single_file_with_dfn3_dereverb_enabled_succeeds() {
    let Some((ffmpeg, _dfn_bin)) = deep_filter_next_to_ffmpeg() else {
        eprintln!("SKIP: deep-filter not found next to ffmpeg on PATH");
        return;
    };
    use vocan::processing::process_single_file;
    use vocan::types::{OutputFormat, ProcessingOptions, ReductionProfile};

    let dir = tempfile::tempdir().unwrap();
    let input_base = dir.path().join("in");
    let output_base = dir.path().join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let input_path = input_base.join("tone.wav");
    write_sine_wav(&input_path, 3.0, 44100, 300.0, 0.4);

    let opts = ProcessingOptions {
        target_lufs: Some(-16.0),
        target_peak_dbfs: -3.0,
        automixer: true,
        automixer_spectral_gate: false,
        automixer_nn_dereverb: false,
        automixer_dfn3_dereverb: true,
        automixer_dfn3_mix: 0.8,
        automixer_dfn3_postfilter: false,
        automixer_expander: false,
        automixer_expander_safety_pct: 50.0,
        automixer_expander_reduction_profile: ReductionProfile::Recommended,
        output_format: OutputFormat::Pcm24Wav,
        bitrate_kbps: 128,
    };

    // Pass the *resolved* ffmpeg path, not a bare "ffmpeg": process_with_rust_dsp
    // looks for deep-filter next to ffmpeg.parent(), which is empty for a bare
    // PATH-relative name (see `deep_filter_next_to_ffmpeg`'s doc comment).
    process_single_file(&input_path, &input_base, &output_base, &opts, &ffmpeg)
        .expect("processing with DFN3 dereverb enabled should succeed");

    let output_path = output_base.join("tone.wav");
    assert!(output_path.exists(), "missing output at {output_path:?}");
}
