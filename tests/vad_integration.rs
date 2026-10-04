//! Silero VAD against the real model, real speech and real ffmpeg.
//!
//! The gate tests in `src/vad.rs` cover the logic around the model with made-up
//! probabilities. These cover the one thing they cannot: that the model, fed
//! through the real decode, finds speech where speech is, and that the cut and
//! the fades come out of the full pipeline as promised.
//!
//! Needs three things, and skips (does not fail) without them:
//!
//! * ffmpeg on PATH,
//! * the model, found through `VOCAN_SILERO_MODEL` or next to the executable,
//! * the speech fixture, through `VOCAN_VAD_FIXTURE` (CI downloads both; see
//!   `.github/workflows/ci.yml`).
//!
//! Run with:
//!
//!   cargo test --test vad_integration -- --ignored
//!
//! # The reference
//!
//! The fixture is Silero's own `tests/data/test.wav`: 60 s of continuous
//! conversational speech. Its boundaries are not guessed. They are what the
//! model reports on it frame by frame, measured with the reference
//! segmentation (threshold 0.5 to enter, 0.35 to leave) and pinned below, so a
//! change in the decode, the framing, the context handling or the model version
//! moves them and fails these tests:
//!
//! ```text
//! speech 0.032 s - 2.016 s
//! speech 2.688 s - 4.672 s
//! speech 4.992 s - 6.848 s
//! speech 9.344 s - 10.272 s   (and on, to 59.552 s)
//! ```
//!
//! The tolerance is two frames (64 ms). The margin the feature adds is 150 ms,
//! so a boundary that wanders by more than that is the boundary being wrong, not
//! the test being strict.

mod common;

use common::{
    noise_at_dbfs, read_wav_samples_f32, rms, skip_if_no_vad, skip_if_no_vad_fixture,
    speech_fixture, speech_samples, wav_secs, write_f32_wav,
};
use std::path::{Path, PathBuf};
use vocan::ffmpeg::{reset_spawn_count, spawn_count};
use vocan::processing::process_single_file;
use vocan::types::{AppMsg, OutputFormat, ProcessingOptions};
use vocan::vad;

/// Two model frames.
const TOLERANCE: f64 = 0.064;
/// Room tone for the lead and the tail: loud enough that the fades are
/// measurable, steady enough that the model does not take it for speech.
const ROOM_DBFS: f32 = -30.0;

fn model() -> PathBuf {
    vad::model_path().expect("skip_if_no_vad checked the model")
}

fn fixture() -> PathBuf {
    speech_fixture().expect("the speech fixture is required (VOCAN_VAD_FIXTURE)")
}

/// A take built from the first `speech_secs` of the fixture, with `lead` seconds
/// of room tone in front and `tail` seconds behind. The fixture is 16 kHz; with
/// `upsample` greater than one every sample is repeated, so the file reaches the
/// pipeline at 16 kHz x `upsample` and the decode has to resample.
fn make_take(
    path: &Path,
    speech_secs: f32,
    lead: f32,
    tail: f32,
    upsample: usize,
) -> (f64, f64, f64) {
    let speech = speech_samples(&fixture(), speech_secs);
    let mut samples = noise_at_dbfs((lead * 16_000.0) as usize, ROOM_DBFS, 7);
    samples.extend_from_slice(&speech);
    samples.extend(noise_at_dbfs((tail * 16_000.0) as usize, ROOM_DBFS, 11));
    let total = samples.len() as f64 / 16_000.0;

    let samples: Vec<f32> = samples
        .into_iter()
        .flat_map(|s| std::iter::repeat_n(s, upsample))
        .collect();
    write_f32_wav(path, &samples, 16_000 * upsample as u32);
    (lead as f64, lead as f64 + speech_secs as f64, total)
}

/// Runs `opts` over `input` and returns the output path and the log lines.
fn run(input: &Path, mut opts: ProcessingOptions) -> (PathBuf, Vec<String>) {
    let dir = input.parent().unwrap();
    let input_base = dir.join("in");
    let output_base = dir.join("out");
    std::fs::create_dir_all(&input_base).unwrap();
    let staged = input_base.join(input.file_name().unwrap());
    std::fs::copy(input, &staged).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    opts.log = Some(tx);
    process_single_file(
        &staged,
        &input_base,
        &output_base,
        &opts,
        &common::ffmpeg_path(),
    )
    .unwrap_or_else(|e| panic!("processing failed: {e:#}"));

    let logs = rx
        .try_iter()
        .filter_map(|m| match m {
            AppMsg::Log(s) => Some(s),
            _ => None,
        })
        .collect();
    let out = output_base
        .join(input.file_name().unwrap())
        .with_extension(OutputFormat::Pcm24Wav.extension());
    (out, logs)
}

fn vad_opts() -> ProcessingOptions {
    ProcessingOptions {
        target_lufs: None,
        trim_silence: true,
        trim_silence_vad: true,
        output_format: OutputFormat::Pcm24Wav,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// detect_speech
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn detects_the_pinned_speech_boundaries_inside_room_tone() {
    if skip_if_no_vad_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("take.wav");
    // First 8 s of the fixture: speech to 6.848 s, then 1.15 s without any.
    let (speech_start, _, total) = make_take(&take, 8.0, 2.0, 2.0, 1);

    let d = vad::detect_speech(&take, &common::ffmpeg_path(), &model()).unwrap();
    let span = d.span.expect("there is speech in this file");

    assert!(
        (d.total_secs - total).abs() < 0.01,
        "decoded length {} vs {total}",
        d.total_secs
    );
    assert!(
        (span.start - (speech_start + 0.032)).abs() <= TOLERANCE,
        "speech start {} s, reference {} s",
        span.start,
        speech_start + 0.032
    );
    assert!(
        (span.end - (speech_start + 6.848)).abs() <= TOLERANCE,
        "speech end {} s, reference {} s",
        span.end,
        speech_start + 6.848
    );
}

#[test]
#[ignore]
fn the_boundaries_do_not_depend_on_the_source_sample_rate() {
    if skip_if_no_vad_fixture() {
        return;
    }
    // The decode resamples to 16 kHz; the answer in seconds must not move.
    let dir = tempfile::tempdir().unwrap();
    let mut found = Vec::new();
    for (name, up) in [("sr16.wav", 1usize), ("sr48.wav", 3)] {
        let take = dir.path().join(name);
        make_take(&take, 8.0, 2.0, 2.0, up);
        let d = vad::detect_speech(&take, &common::ffmpeg_path(), &model()).unwrap();
        found.push(d.span.expect("speech"));
    }
    assert!(
        (found[0].start - found[1].start).abs() <= TOLERANCE
            && (found[0].end - found[1].end).abs() <= TOLERANCE,
        "16 kHz {:?} vs 48 kHz {:?}",
        found[0],
        found[1]
    );
}

#[test]
#[ignore]
fn quiet_speech_is_found_where_it_starts() {
    if skip_if_no_vad_fixture() {
        return;
    }
    // Speech 40 dB under the fixture, over a room floor 60 dB under it. Without
    // the level evening in the detection decode the model finds the start
    // 0.27 s late here, which eats the first word; with it, the start lands on
    // the pinned reference like any other take.
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("quiet.wav");
    let gain = 10f32.powf(-40.0 / 20.0);
    let mut samples = noise_at_dbfs(16_000 * 2, -75.0, 13);
    samples.extend(
        speech_samples(&fixture(), 8.0)
            .into_iter()
            .map(|s| s * gain),
    );
    samples.extend(noise_at_dbfs(16_000 * 2, -75.0, 17));
    write_f32_wav(&take, &samples, 16_000);

    let d = vad::detect_speech(&take, &common::ffmpeg_path(), &model()).unwrap();
    let span = d.span.expect("quiet speech is still speech");
    assert!(
        (span.start - 2.032).abs() <= TOLERANCE,
        "quiet speech starts at 2.032 s, found at {} s",
        span.start
    );
    assert!(
        (span.end - 8.848).abs() <= TOLERANCE,
        "quiet speech ends at 8.848 s, found at {} s",
        span.end
    );
}

#[test]
#[ignore]
fn a_stereo_source_is_heard_as_mono() {
    if skip_if_no_vad_fixture() {
        return;
    }
    // Without the downmix the decode would hand the model interleaved L/R
    // samples: the same speech at half speed with a doubled pitch. The span
    // would not be wrong by a frame, it would be wrong by seconds.
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("stereo.wav");
    let mut mono = noise_at_dbfs(16_000 * 2, ROOM_DBFS, 7);
    mono.extend(speech_samples(&fixture(), 8.0));
    mono.extend(noise_at_dbfs(16_000 * 2, ROOM_DBFS, 11));

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 16_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&take, spec).unwrap();
    for &s in &mono {
        w.write_sample(s).unwrap();
        w.write_sample(s * 0.5).unwrap();
    }
    w.finalize().unwrap();

    let d = vad::detect_speech(&take, &common::ffmpeg_path(), &model()).unwrap();
    let span = d.span.expect("speech");
    assert!(
        (d.total_secs - 12.0).abs() < 0.01,
        "length {}",
        d.total_secs
    );
    assert!(
        (span.start - 2.032).abs() <= TOLERANCE && (span.end - 8.848).abs() <= TOLERANCE,
        "stereo take: {span:?}"
    );
}

#[test]
#[ignore]
fn finds_no_speech_in_noise_or_silence() {
    if skip_if_no_vad() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (name, samples) in [
        ("room.wav", noise_at_dbfs(16_000 * 5, ROOM_DBFS, 3)),
        ("loud_noise.wav", noise_at_dbfs(16_000 * 5, -12.0, 5)),
        ("silence.wav", vec![0.0f32; 16_000 * 5]),
    ] {
        let path = dir.path().join(name);
        write_f32_wav(&path, &samples, 16_000);
        let d = vad::detect_speech(&path, &common::ffmpeg_path(), &model()).unwrap();
        assert_eq!(d.span, None, "{name} must not contain speech");
        assert!((d.total_secs - 5.0).abs() < 0.01, "{name}");
    }
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// What every pipeline test checks about a trimmed take: the length, and the
/// shape of the two edges.
fn assert_trimmed(out: &Path, expected_secs: f64, tolerance: f64, what: &str) {
    let secs = wav_secs(out);
    assert!(
        (secs - expected_secs).abs() <= tolerance,
        "{what}: expected {expected_secs:.3} s, got {secs:.3} s"
    );

    let samples = read_wav_samples_f32(out);
    let sr = hound::WavReader::open(out).unwrap().spec().sample_rate as usize;
    let ms = |m: usize| sr * m / 1000;

    // The kept lead is 150 ms of room tone with a 75 ms fade across its first
    // half: the first 10 ms must be far quieter than the stretch just after the
    // fade has finished (80-140 ms), which is plain room tone.
    let head_start = rms(&samples[..ms(10)]);
    let head_plateau = rms(&samples[ms(80)..ms(140)]);
    assert!(
        head_start < head_plateau * 0.35,
        "{what}: no fade-in (first 10 ms {head_start:.5}, plateau {head_plateau:.5})"
    );

    let n = samples.len();
    let tail_end = rms(&samples[n - ms(10)..]);
    let tail_plateau = rms(&samples[n - ms(140)..n - ms(80)]);
    assert!(
        tail_end < tail_plateau * 0.35,
        "{what}: no fade-out (last 10 ms {tail_end:.5}, plateau {tail_plateau:.5})"
    );
}

/// Speech 0.032-6.848 s of the 8 s excerpt; with 2 s of lead the speech runs
/// from 2.032 to 8.848, and the kept audio is 150 ms wider on each side.
const KEPT_SECS: f64 = (8.848 + 0.150) - (2.032 - 0.150);

#[test]
#[ignore]
fn plain_pipeline_keeps_speech_plus_margins_and_fades_the_edges() {
    if skip_if_no_vad_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("take.wav");
    make_take(&take, 8.0, 2.0, 2.0, 1);

    let (out, logs) = run(&take, vad_opts());
    assert_trimmed(&out, KEPT_SECS, 0.11, "plain");
    assert!(
        logs.iter().any(|l| l.contains("speech")),
        "the log should say what was found: {logs:?}"
    );
}

#[test]
#[ignore]
fn automixer_pipeline_trims_the_same_way() {
    if skip_if_no_vad_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("take.wav");
    // 48 kHz, not the fixture's native 16 kHz: the Automixer's voice EQ has a
    // band at 8 kHz, which sits on the Nyquist frequency of a 16 kHz signal and
    // makes the pipeline fail with "biquad coeffs failed for band 3" -- a
    // limitation of the Automixer that has nothing to do with the trim.
    make_take(&take, 8.0, 2.0, 2.0, 3);

    let mut opts = vad_opts();
    opts.automixer = true;
    opts.automixer_spectral_gate = true;
    let (out, _) = run(&take, opts);
    // The room tone is partly removed by the gate, which flattens the fade
    // measurement; the length is what must agree with the plain pipeline.
    let secs = wav_secs(&out);
    assert!(
        (secs - KEPT_SECS).abs() <= 0.11,
        "Automixer + VAD: expected {KEPT_SECS:.3} s, got {secs:.3} s"
    );
}

#[test]
#[ignore]
fn a_48_khz_source_is_trimmed_to_the_same_length() {
    if skip_if_no_vad_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("take48.wav");
    make_take(&take, 8.0, 2.0, 2.0, 3);

    let (out, _) = run(&take, vad_opts());
    assert_trimmed(&out, KEPT_SECS, 0.11, "48 kHz");
}

#[test]
#[ignore]
fn the_kept_speech_is_not_touched() {
    if skip_if_no_vad_fixture() {
        return;
    }
    // The fades live in the margins. The loudness of the speech itself must be
    // what it was: compare the RMS of the speech stretch before and after.
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("take.wav");
    make_take(&take, 8.0, 2.0, 2.0, 1);

    let (out, _) = run(&take, vad_opts());
    let samples = read_wav_samples_f32(&out);
    let sr = hound::WavReader::open(&out).unwrap().spec().sample_rate as usize;

    let original = speech_samples(&fixture(), 8.0);
    // Speech occupies 0.032-6.848 s of the excerpt; in the output it starts
    // 0.150 s in. Compare the middle 5 s, clear of both edges.
    let want = rms(&original[16_000..16_000 * 6]);
    let from = sr * 150 / 1000 + sr;
    let got = rms(&samples[from..from + sr * 5]);
    assert!(
        (got / want - 1.0).abs() < 0.05,
        "speech RMS moved: {want:.4} -> {got:.4}"
    );
}

#[test]
#[ignore]
fn a_take_without_speech_is_left_exactly_as_it_would_be_with_the_trim_off() {
    if skip_if_no_vad() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("hum.wav");
    write_f32_wav(&take, &noise_at_dbfs(16_000 * 4, ROOM_DBFS, 9), 16_000);

    let (with_vad, logs) = run(&take, vad_opts());

    let off_dir = tempfile::tempdir().unwrap();
    let take_off = off_dir.path().join("hum.wav");
    std::fs::copy(&take, &take_off).unwrap();
    let mut off = vad_opts();
    off.trim_silence = false;
    off.trim_silence_vad = false;
    let (without, _) = run(&take_off, off);

    assert_eq!(
        read_wav_samples_f32(&with_vad),
        read_wav_samples_f32(&without),
        "no speech must mean sample-identical output"
    );
    assert!(
        logs.iter().any(|l| l.contains("no speech")),
        "the log must say why nothing was trimmed: {logs:?}"
    );
}

#[test]
#[ignore]
fn a_very_quiet_take_is_not_rejected_by_the_threshold_guard() {
    if skip_if_no_vad() {
        return;
    }
    // The threshold trim refuses a file whose loudest moment is under its
    // threshold, because trimming it would leave nothing. That guard has no
    // business with VAD, which leaves a speechless file alone. Room tone at
    // -70 dBFS is far below every threshold preset; with VAD on, the Automixer
    // must process it, not fail with "nothing in this file reaches the Trim
    // silence threshold".
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("whisper.wav");
    write_f32_wav(&take, &noise_at_dbfs(48_000 * 4, -70.0, 21), 48_000);

    let mut opts = vad_opts();
    opts.automixer = true;
    opts.automixer_spectral_gate = true;
    let (out, logs) = run(&take, opts);
    assert!(
        (wav_secs(&out) - 4.0).abs() < 0.05,
        "a file with no speech must come through whole"
    );
    assert!(logs.iter().any(|l| l.contains("no speech")), "{logs:?}");
}

#[test]
#[ignore]
fn pauses_inside_the_line_survive() {
    if skip_if_no_vad_fixture() {
        return;
    }
    // Two phrases with 1.5 s of room tone between them. The trim must follow
    // the outer edges only.
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("two.wav");
    let first = speech_samples(&fixture(), 2.1);
    let mut samples = noise_at_dbfs(16_000 * 2, ROOM_DBFS, 1);
    samples.extend_from_slice(&first);
    samples.extend(noise_at_dbfs(16_000 * 3 / 2, ROOM_DBFS, 2));
    samples.extend_from_slice(&first);
    samples.extend(noise_at_dbfs(16_000 * 2, ROOM_DBFS, 3));
    write_f32_wav(&take, &samples, 16_000);

    let (out, _) = run(&take, vad_opts());
    // Both phrases and the pause between them, plus 150 ms each side, less the
    // part of the second phrase the model does not count (its last frames).
    let expected = 0.150 + 2.1 + 1.5 + 2.1 + 0.150;
    let secs = wav_secs(&out);
    assert!(
        (secs - expected).abs() <= 0.35,
        "the pause was cut or the second phrase lost: expected ~{expected:.2} s, got {secs:.2} s"
    );
}

// ---------------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn speech_detection_costs_exactly_one_extra_ffmpeg_process() {
    if skip_if_no_vad_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("take.wav");
    make_take(&take, 8.0, 2.0, 2.0, 1);

    let spawns = |opts: ProcessingOptions| {
        reset_spawn_count();
        run(&take, opts);
        spawn_count()
    };
    let threshold = ProcessingOptions {
        trim_silence_vad: false,
        ..vad_opts()
    };
    assert_eq!(spawns(vad_opts()), spawns(threshold) + 1);
}
