//! Speech-aware silence trimming with Silero VAD.
//!
//! The classic trim (`processing::trim_silence_chain`) asks FFmpeg's
//! `silenceremove` where the signal drops below a level. That cannot tell a
//! breath or a room tone from a word, so on a noisy take it either finds no
//! silence at all or bites into quiet consonants. This module asks a neural
//! network instead: [Silero VAD](https://github.com/snakers4/silero-vad) scores
//! every 32 ms of the take for "is this speech", and the trim follows the speech.
//!
//! The pipeline is three small steps, deliberately kept apart so that all but
//! the first can be tested without a model:
//!
//! 1. [`detect_speech`] decodes the file to 16 kHz mono in **one extra ffmpeg
//!    process** (two for a file that is quiet as a whole, which is decoded a
//!    second time with a gain), streams it through the model frame by frame and
//!    returns the first start and last end of speech. Only per-frame numbers are
//!    kept, never the audio, so memory use grows by about a megabyte per
//!    hour of audio.
//! 2. [`plan_trim`] widens that span by [`MARGIN_SECS`] on each side and works
//!    out the fades.
//! 3. [`trim_filter`] turns the plan into an `atrim`/`afade` filter string that
//!    goes where the `silenceremove` chain would have gone.
//!
//! The decode is independent of everything else in the pipeline. It reads the
//! original file, writes nothing, and the audio that is finally encoded differs
//! from the non-VAD result only by the cut and the two fades.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::ffmpeg::ffmpeg_cmd;
use crate::proc;

/// The sample rate the model is fed at. Silero accepts 8 kHz and 16 kHz; 16 kHz
/// is the one with the better accuracy.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per model frame at [`SAMPLE_RATE`] (32 ms). The model only accepts
/// this size at 16 kHz.
pub const WINDOW: usize = 512;
/// Samples of the previous frame prepended to each model input. The v5/v6
/// models expect this context. `probabilities_match_the_reference_run` pins the
/// model's output on real speech, so dropping or shifting it fails a test.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
const CONTEXT: usize = 64;
/// A silent run turns into speech when a frame scores at least this.
pub const START_THRESHOLD: f32 = 0.5;
/// Speech ends when a frame scores below this. Lower than the start threshold
/// so that one weak frame in the middle of a word does not end the speech
/// (Silero's own hysteresis: `threshold - 0.15`).
pub const END_THRESHOLD: f32 = 0.35;
/// Speech bursts shorter than this are ignored, so a door slam or a mouth click
/// does not become the start of the take.
pub const MIN_SPEECH_SECS: f64 = 0.25;
/// Speech only ends after this much uninterrupted quiet. One or two weak frames
/// inside a word -- a stop consonant, a breath between syllables -- are common
/// and must not cut the word in two. Silero's reference uses
/// `min_silence_duration_ms = 100`, checked from the first quiet frame: with
/// 32 ms frames that is four frames after it, so a pause closes the speech when
/// it is five quiet frames long (160 ms). Fewer would split a short leading word
/// from the rest of the line on a pause the reference bridges, and the word
/// would then be dropped as too short to count.
pub const MIN_SILENCE_SECS: f64 = 0.100;

/// A file whose level is below this (-20 dBFS) is read a second time with a
/// gain, see [`rescue_gain_db`].
///
/// Silero's scores depend on level. Speech recorded 40 dB under a normal take
/// was "found" 0.27 s late in testing, and 45 dB under it over a second late,
/// so a quiet voice-over lost the opening of its first word to the cut.
///
/// "Level" is the 99th percentile of the per-frame peaks, not the largest
/// sample: one bump of the microphone or a click is a single frame, and gating
/// on the largest sample let it switch the rescue off for a take that was
/// otherwise 40 dB too quiet (the cut then landed up to a second inside the
/// speech). The threshold is generous on purpose; boosting a file that did not
/// strictly need it was tested on white, pink and brown noise, hum and breaths
/// and found no speech that was not there.
const QUIET_LEVEL: f32 = 0.1;
/// The level the second read is brought to (-10.5 dBFS): a normal level.
const QUIET_TARGET_LEVEL: f32 = 0.3;
/// Ceiling on that gain. Past it the file is noise-floor-sized and boosting it
/// further finds only the noise.
const QUIET_MAX_GAIN_DB: f32 = 40.0;
// Why a second read and not a leveller (`dynaudnorm`) in the first: a leveller
// lifts every pause to the level of the speech as well, and on a real take the
// model then calls the breaths and room noise in the pauses speech -- on the
// test recording the end of the speech moved a full second. A single gain on
// a file that is quiet as a whole raises speech and pause together, so their
// relation, which is what the model judges, is unchanged. It costs a second
// process only for files that are quiet to begin with.

/// The gain, in dB, for the second read of a file whose level is `level`, or
/// `None` when it should not be read again: it is loud enough already, or it is
/// digital silence (a gain cannot find speech in zeros).
fn rescue_gain_db(level: f32) -> Option<f32> {
    if level.is_nan() || level <= 0.0 || level >= QUIET_LEVEL {
        return None;
    }
    Some((20.0 * (QUIET_TARGET_LEVEL / level).log10()).min(QUIET_MAX_GAIN_DB))
}

/// The 99th percentile of `peaks` (0 for an empty slice).
fn level_of(peaks: &[f32]) -> f32 {
    if peaks.is_empty() {
        return 0.0;
    }
    let mut sorted = peaks.to_vec();
    sorted.sort_by(f32::total_cmp);
    sorted[((sorted.len() - 1) as f64 * 0.99).floor() as usize]
}
/// Audio kept before the first and after the last speech.
pub const MARGIN_SECS: f64 = 0.150;
/// Length of the fade-in and fade-out. A fixed value, but never longer than the
/// margin that is actually available (see [`plan_trim`]).
pub const FADE_SECS: f64 = 0.075;

/// The model file. Pinned to a release tag, not `master`, and verified by hash;
/// the installers, CI and `tests/installer_consistency.rs` all use these two
/// values, so a drift between them fails a test.
pub const MODEL_URL: &str =
    "https://github.com/snakers4/silero-vad/raw/v6.2.2/src/silero_vad/data/silero_vad.onnx";
/// SHA-256 of [`MODEL_URL`], lowercase hex.
pub const MODEL_SHA256: &str = "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3";
/// File name the model is stored under.
pub const MODEL_FILE: &str = "silero_vad.onnx";
/// Environment variable that points at a model file and wins over the
/// default locations.
pub const MODEL_ENV: &str = "VOCAN_SILERO_MODEL";

/// `false` on targets for which the `ort` crate ships no prebuilt ONNX Runtime
/// (Intel macOS). There the option stays visible but cannot be switched on.
pub const ENGINE_AVAILABLE: bool = cfg!(not(all(target_os = "macos", target_arch = "x86_64")));

// ---------------------------------------------------------------------------
// Pure logic: segmentation, trim plan, filter string
// ---------------------------------------------------------------------------

/// Where speech starts and ends in a take, in seconds from its first sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeechSpan {
    pub start: f64,
    pub end: f64,
}

/// What [`detect_speech`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    /// `None` when the model found no speech at all.
    pub span: Option<SpeechSpan>,
    /// Length of the decoded file, in seconds.
    pub total_secs: f64,
}

/// Seconds covered by one model frame.
fn frame_secs() -> f64 {
    WINDOW as f64 / SAMPLE_RATE as f64
}

/// From per-frame speech probabilities to the first start and the last end of
/// speech, or `None` when there is none.
///
/// Segments come from a two-threshold state machine ([`START_THRESHOLD`] to
/// enter, [`END_THRESHOLD`] to leave). A segment shorter than
/// [`MIN_SPEECH_SECS`] is dropped. The result runs from the start of the first
/// surviving segment to the end of the last one, so pauses inside the line stay
/// inside the span -- the same contract as the threshold trim, which never
/// looks past the first sound it finds.
///
/// `total_secs` caps the end, because the last frame is zero-padded and its
/// nominal end can lie past the real end of the file.
pub fn speech_span(probs: &[f32], total_secs: f64) -> Option<SpeechSpan> {
    let min_frames = (MIN_SPEECH_SECS / frame_secs()).ceil() as usize;
    let min_silence_frames = (MIN_SILENCE_SECS / frame_secs()).ceil() as usize;
    let mut first_start: Option<usize> = None;
    let mut last_end: Option<usize> = None;
    let mut keep = |s: usize, e: usize| {
        if e - s >= min_frames {
            first_start.get_or_insert(s);
            last_end = Some(e);
        }
    };

    // `open` is where the current segment began; `quiet_from` is where the
    // current run of below-`END_THRESHOLD` frames began, if there is one. The
    // segment closes at `quiet_from` -- the first quiet frame, not the one on
    // which the run turned long enough to count -- so the end is not pushed
    // out by the time it took to be sure.
    let mut open: Option<usize> = None;
    let mut quiet_from: Option<usize> = None;
    for (i, &p) in probs.iter().enumerate() {
        match open {
            None if p >= START_THRESHOLD => open = Some(i),
            Some(s) => {
                if p >= END_THRESHOLD {
                    quiet_from = None;
                } else {
                    let from = *quiet_from.get_or_insert(i);
                    // `i - from`, not `i + 1 - from`: Silero's reference closes
                    // a segment when a quiet frame lies `min_silence` after the
                    // *first* quiet one, so the run has to be that many frames
                    // long *plus the first*. One frame less would split a
                    // leading word from the rest on a pause the reference
                    // bridges, and then drop it as too short.
                    if i - from >= min_silence_frames {
                        keep(s, from);
                        open = None;
                        quiet_from = None;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(s) = open {
        // The file ended before the pause got long enough to count; whatever
        // quiet run was under way is the tail of the file, not of the speech.
        keep(s, quiet_from.unwrap_or(probs.len()));
    }

    let (s, e) = (first_start?, last_end?);
    Some(SpeechSpan {
        start: s as f64 * frame_secs(),
        end: (e as f64 * frame_secs()).min(total_secs),
    })
}

/// The cut and the fades, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimPlan {
    /// Where the kept audio starts and ends.
    pub start: f64,
    pub end: f64,
    pub fade_in: f64,
    pub fade_out: f64,
}

/// Widens `span` by [`MARGIN_SECS`] on each side (never past the ends of the
/// file) and sizes the fades.
///
/// The fades sit **inside the margins**, so they never touch detected speech: a
/// fade is [`FADE_SECS`] long, or as long as the margin that was actually
/// available when the speech starts closer than that to the edge of the file.
/// Speech at the very first sample gets no fade-in at all, which is right --
/// nothing was cut there, so there is nothing to soften.
pub fn plan_trim(span: SpeechSpan, total_secs: f64) -> TrimPlan {
    let start = (span.start - MARGIN_SECS).max(0.0);
    let end = (span.end + MARGIN_SECS).min(total_secs);
    TrimPlan {
        start,
        end,
        fade_in: FADE_SECS.min(span.start - start).max(0.0),
        fade_out: FADE_SECS.min(end - span.end).max(0.0),
    }
}

/// Differences below this are rounding noise from the sample-count based
/// duration, not a real cut or a real fade.
const EPS: f64 = 0.001;

/// The ffmpeg filter chain for `plan`, or `None` when it would change nothing
/// (the whole file kept and no fade to apply).
///
/// A leading `asetpts=PTS-STARTPTS` makes the first decoded sample time zero
/// whatever offset the container carries, which is the time base the VAD
/// decode used. The trailing one restarts the clock after the cut so that
/// `afade`'s `st` and everything downstream count from zero.
///
/// One literal chain with no whitespace: it is passed as a single `-af`
/// argument, and `processing::tests::trim_chain_is_a_single_ffmpeg_argument`
/// guards the same property for the threshold trim.
pub fn trim_filter(plan: &TrimPlan, total_secs: f64) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    let cut_head = plan.start > EPS;
    let cut_tail = plan.end < total_secs - EPS;
    if cut_head || cut_tail {
        let mut atrim = String::from("atrim=");
        if cut_head {
            atrim.push_str(&format!("start={:.3}", plan.start));
        }
        if cut_tail {
            if cut_head {
                atrim.push(':');
            }
            atrim.push_str(&format!("end={:.3}", plan.end));
        }
        parts.push("asetpts=PTS-STARTPTS".to_string());
        parts.push(atrim);
        parts.push("asetpts=PTS-STARTPTS".to_string());
    }

    let kept = plan.end - plan.start;
    if plan.fade_in > EPS {
        parts.push(format!("afade=t=in:st=0:d={:.3}", plan.fade_in));
    }
    if plan.fade_out > EPS {
        parts.push(format!(
            "afade=t=out:st={:.3}:d={:.3}",
            (kept - plan.fade_out).max(0.0),
            plan.fade_out
        ));
    }

    (!parts.is_empty()).then(|| parts.join(","))
}

// ---------------------------------------------------------------------------
// Model location
// ---------------------------------------------------------------------------

/// Where the model is, if it is anywhere VOCAN looks.
///
/// Order: the file named by [`MODEL_ENV`] (a path that does not exist is
/// skipped, not trusted), then `models/silero_vad.onnx` next to the executable
/// (where the installers put it), then `silero_vad.onnx` next to the
/// executable.
pub fn model_path() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    model_path_in(std::env::var_os(MODEL_ENV), exe_dir.as_deref())
}

fn model_path_in(env: Option<std::ffi::OsString>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    let from_env = env
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    from_env.or_else(|| {
        let dir = exe_dir?;
        [dir.join("models").join(MODEL_FILE), dir.join(MODEL_FILE)]
            .into_iter()
            .find(|p| p.is_file())
    })
}

/// Whether Silero VAD can be used right now: the engine exists for this target
/// and the model file was found.
pub fn available() -> bool {
    ENGINE_AVAILABLE && model_path().is_some()
}

/// What to tell the user when [`available`] is `false`.
pub fn unavailable_reason() -> String {
    if !ENGINE_AVAILABLE {
        "Silero VAD is not available on Intel Macs. Trim by level instead.".to_owned()
    } else {
        format!(
            "Silero VAD needs {MODEL_FILE} in a models folder next to VOCAN. Re-run the \
             installer to download it, then restart VOCAN."
        )
    }
}

// ---------------------------------------------------------------------------
// Streaming a decode through the model
// ---------------------------------------------------------------------------

/// Reads a raw little-endian f32 stream and calls `on_frame` once per
/// [`WINDOW`] samples. A partial final frame is zero-padded, the way Silero's
/// own reference implementation does it. Returns the number of real samples.
///
/// `Read::read` makes no promise about landing on a 4-byte boundary, so a
/// 0-3 byte remainder is carried across reads.
fn for_each_frame(
    reader: &mut impl Read,
    mut on_frame: impl FnMut(&[f32]) -> Result<()>,
) -> Result<u64> {
    let mut buf = vec![0u8; 1 << 16];
    let mut rem = [0u8; 4];
    let mut rem_len = 0usize;
    let mut frame = [0f32; WINDOW];
    let mut filled = 0usize;
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            rem[rem_len] = byte;
            rem_len += 1;
            if rem_len == 4 {
                rem_len = 0;
                frame[filled] = f32::from_le_bytes(rem);
                filled += 1;
                total += 1;
                if filled == WINDOW {
                    on_frame(&frame)?;
                    filled = 0;
                }
            }
        }
    }
    if filled > 0 {
        frame[filled..].fill(0.0);
        on_frame(&frame)?;
    }
    Ok(total)
}

/// Finds the speech in `input`.
///
/// Costs one ffmpeg process (counted by `ffmpeg::spawn_count`, pinned by
/// `tests/spawn_budget.rs`), and a second one for a take that is quiet as a
/// whole (see [`QUIET_LEVEL`]). The children are registered with
/// [`proc::register`], so Stop kills them mid-file like any other pass.
pub fn detect_speech(input: &Path, ffmpeg: &Path, model: &Path) -> Result<Detection> {
    let mut pass = run_pass(input, ffmpeg, model, None)?;

    if let Some(gain_db) = rescue_gain_db(pass.level) {
        pass = run_pass(input, ffmpeg, model, Some(gain_db))?;
    }

    let total_secs = pass.samples as f64 / SAMPLE_RATE as f64;
    Ok(Detection {
        span: speech_span(&pass.probs, total_secs),
        total_secs,
    })
}

/// One decode of `input` through the model.
struct Pass {
    /// Speech probability of every frame.
    probs: Vec<f32>,
    /// Real samples decoded (the zero padding of the last frame excluded).
    samples: u64,
    /// The level of what the model was fed in this pass (see [`QUIET_LEVEL`]).
    level: f32,
}

/// Decodes `input` to 16 kHz mono, optionally with `gain_db` applied, and runs
/// the model over it frame by frame.
fn run_pass(input: &Path, ffmpeg: &Path, model: &Path, gain_db: Option<f32>) -> Result<Pass> {
    // Load first: a missing or corrupt model should fail before a process is
    // spawned for nothing.
    let mut silero = engine::Silero::load(model)?;

    // Downmix first, then gain: the model hears the mono sum, and that is the
    // signal whose level matters.
    let filter = match gain_db {
        Some(db) => format!("aformat=channel_layouts=mono,volume={db:.2}dB:precision=float"),
        None => "aformat=channel_layouts=mono".to_string(),
    };

    let mut child = ffmpeg_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter])
        .args(["-ac", "1", "-ar", &SAMPLE_RATE.to_string()])
        .args(["-f", "f32le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("FFmpeg spawn failed (speech detection pass)")?;

    // Declared after `child` so it drops first; see `proc::register`.
    let _child_guard = proc::register(&child);

    // Drained on its own thread so a chatty ffmpeg cannot fill the pipe and
    // stall while we are busy in the model.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    // Same teardown order as the de-esser pass in `processing.rs`: take the
    // result, close our read end so a still-writing ffmpeg cannot block the
    // `wait()`, reap, and only then propagate any error.
    let mut probs: Vec<f32> = Vec::new();
    let mut frame_peaks: Vec<f32> = Vec::new();
    let read_result = match child.stdout.as_mut() {
        Some(stdout) => for_each_frame(stdout, |frame| {
            // An explicit comparison rather than `f32::max`: a NaN sample
            // compares false, so it can never become the peak.
            frame_peaks.push(
                frame
                    .iter()
                    .fold(0.0f32, |m, s| if s.abs() > m { s.abs() } else { m }),
            );
            probs.push(silero.probability(frame)?);
            Ok(())
        }),
        None => Err(anyhow!("FFmpeg stdout not piped")),
    };
    drop(child.stdout.take());
    let status = child.wait();
    let stderr_text = stderr_handle.join().unwrap_or_default();

    let samples = read_result.context("running Silero VAD over the decoded audio")?;
    let status = status.context("FFmpeg speech detection wait failed")?;
    if !status.success() {
        // The shared helper keeps the last 600 characters, where the reason is,
        // and says something useful when stderr is empty.
        return Err(crate::ffmpeg::ffmpeg_failed(
            "speech detection",
            &stderr_text,
        ));
    }
    if samples == 0 {
        return Err(anyhow!("the file decoded to no audio at all"));
    }

    Ok(Pass {
        probs,
        samples,
        level: level_of(&frame_peaks),
    })
}

// ---------------------------------------------------------------------------
// The model itself
// ---------------------------------------------------------------------------

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
mod engine {
    use super::{CONTEXT, SAMPLE_RATE, WINDOW};
    use anyhow::{anyhow, Result};
    use ort::session::Session;
    use ort::value::Tensor;
    use std::path::Path;

    /// `ort::Error` carries the session builder and is not `Send + Sync`, so it
    /// cannot go through `?` into `anyhow::Error`; keep the message instead.
    fn msg<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> anyhow::Error {
        move |e| anyhow!("Silero VAD: {what}: {e}")
    }

    /// One Silero VAD session plus the recurrent state it carries from frame to
    /// frame. Built per file: the model is 2 MB and loads in a few
    /// milliseconds, and a session per file keeps files independent when rayon
    /// runs them in parallel.
    pub struct Silero {
        session: Session,
        /// LSTM state, `[2, 1, 128]`.
        state: Vec<f32>,
        /// Tail of the previous frame, prepended to the next.
        context: [f32; CONTEXT],
    }

    impl Silero {
        pub fn load(model: &Path) -> Result<Self> {
            let session = Session::builder()
                .map_err(msg("cannot create session"))?
                // Files are processed in parallel; one thread per session
                // keeps `cores - 1` of them from oversubscribing the CPU.
                .with_intra_threads(1)
                .map_err(msg("cannot configure session"))?
                .commit_from_file(model)
                .map_err(msg("cannot load the model"))?;
            Ok(Self {
                session,
                state: vec![0.0; 2 * 128],
                context: [0.0; CONTEXT],
            })
        }

        /// Speech probability of one [`WINDOW`]-sample frame.
        pub fn probability(&mut self, frame: &[f32]) -> Result<f32> {
            debug_assert_eq!(frame.len(), WINDOW);
            let mut input = Vec::with_capacity(CONTEXT + WINDOW);
            input.extend_from_slice(&self.context);
            input.extend_from_slice(frame);
            self.context.copy_from_slice(&frame[WINDOW - CONTEXT..]);

            let outputs = self
                .session
                .run(ort::inputs![
                    "input" => Tensor::from_array(([1usize, CONTEXT + WINDOW], input))
                        .map_err(msg("cannot build the input tensor"))?,
                    "state" => Tensor::from_array(([2usize, 1, 128], self.state.clone()))
                        .map_err(msg("cannot build the state tensor"))?,
                    "sr" => Tensor::from_array(((), vec![i64::from(SAMPLE_RATE)]))
                        .map_err(msg("cannot build the sample-rate tensor"))?,
                ])
                .map_err(msg("inference failed"))?;

            let (_, prob) = outputs["output"]
                .try_extract_tensor::<f32>()
                .map_err(msg("unexpected model output"))?;
            let prob = *prob
                .first()
                .ok_or_else(|| anyhow!("Silero VAD: the model returned no score"))?;
            let (_, next) = outputs["stateN"]
                .try_extract_tensor::<f32>()
                .map_err(msg("unexpected model state"))?;
            if next.len() != self.state.len() {
                return Err(anyhow!(
                    "Silero VAD: unexpected state size {} (wanted {})",
                    next.len(),
                    self.state.len()
                ));
            }
            self.state.copy_from_slice(next);
            Ok(prob)
        }
    }
}

/// Intel macOS has no prebuilt ONNX Runtime from the `ort` crate, so there the
/// engine is a stub that says so. The rest of the module still compiles and
/// its tests still run.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
mod engine {
    use anyhow::{anyhow, Result};
    use std::path::Path;

    pub struct Silero;

    impl Silero {
        pub fn load(_model: &Path) -> Result<Self> {
            Err(anyhow!(
                "Silero VAD is not available on Intel Macs (no ONNX Runtime build for this platform)"
            ))
        }

        pub fn probability(&mut self, _frame: &[f32]) -> Result<f32> {
            unreachable!("Silero::load never succeeds on this platform")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const F: f64 = 512.0 / 16_000.0;

    /// `n` frames at probability `p`.
    fn run(p: f32, n: usize) -> Vec<f32> {
        vec![p; n]
    }

    fn probs(parts: &[(f32, usize)]) -> Vec<f32> {
        parts.iter().flat_map(|&(p, n)| run(p, n)).collect()
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // ---- speech_span ------------------------------------------------------

    #[test]
    fn no_speech_gives_none() {
        assert_eq!(speech_span(&run(0.02, 100), 3.2), None);
        assert_eq!(speech_span(&[], 0.0), None);
    }

    #[test]
    fn finds_the_edges_of_a_single_phrase() {
        // 10 silent frames, 20 speech frames, 10 silent frames.
        let p = probs(&[(0.01, 10), (0.9, 20), (0.01, 10)]);
        let s = speech_span(&p, 40.0 * F).unwrap();
        assert!(near(s.start, 10.0 * F));
        assert!(near(s.end, 30.0 * F));
    }

    #[test]
    fn a_pause_inside_the_line_stays_inside_the_span() {
        let p = probs(&[(0.01, 5), (0.9, 12), (0.01, 30), (0.9, 12), (0.01, 5)]);
        let s = speech_span(&p, 64.0 * F).unwrap();
        assert!(near(s.start, 5.0 * F));
        assert!(
            near(s.end, 59.0 * F),
            "the span must reach the second phrase"
        );
    }

    #[test]
    fn a_short_burst_before_the_line_is_not_its_start() {
        // 3 frames (96 ms) of click, silence, then a real phrase.
        let p = probs(&[(0.9, 3), (0.01, 20), (0.9, 15), (0.01, 5)]);
        let s = speech_span(&p, 43.0 * F).unwrap();
        assert!(near(s.start, 23.0 * F), "start was {}", s.start);
    }

    #[test]
    fn a_short_burst_after_the_line_is_not_its_end() {
        let p = probs(&[(0.01, 5), (0.9, 15), (0.01, 20), (0.9, 3), (0.01, 5)]);
        let s = speech_span(&p, 48.0 * F).unwrap();
        assert!(near(s.end, 20.0 * F), "end was {}", s.end);
    }

    #[test]
    fn a_dip_shorter_than_the_minimum_silence_does_not_split_speech() {
        // Up to four quiet frames (128 ms) inside a line: not a pause.
        for dip in [1usize, 2, 3, 4] {
            let p = probs(&[(0.01, 4), (0.9, 12), (0.1, dip), (0.9, 12), (0.01, 4)]);
            let total = p.len() as f64 * F;
            let s = speech_span(&p, total).unwrap();
            assert!(near(s.start, 4.0 * F), "dip {dip}: start {}", s.start);
            assert!(
                near(s.end, (4 + 12 + dip + 12) as f64 * F),
                "dip {dip}: end {}",
                s.end
            );
        }
    }

    #[test]
    fn a_dip_in_the_first_word_does_not_hide_its_onset() {
        // The scenario that motivated the minimum silence: without it the first
        // five frames are a fragment too short to count, the span starts at the
        // second fragment, and the onset of the take falls outside the margin.
        let p = probs(&[(0.01, 6), (0.9, 5), (0.3, 1), (0.9, 10), (0.01, 6)]);
        let s = speech_span(&p, p.len() as f64 * F).unwrap();
        assert!(near(s.start, 6.0 * F), "start was {}", s.start);
    }

    #[test]
    fn five_quiet_frames_end_the_speech_at_the_first_of_them() {
        let p = probs(&[(0.01, 4), (0.9, 12), (0.1, 5), (0.9, 3), (0.01, 4)]);
        // The 3-frame burst after the pause is too short to count, so the span
        // ends where the pause began.
        let s = speech_span(&p, p.len() as f64 * F).unwrap();
        assert!(near(s.end, 16.0 * F), "end was {}", s.end);
    }

    #[test]
    fn the_minimum_pause_is_exactly_five_frames() {
        // Two halves that are each too short to count (5 frames) but together
        // are speech (>= 8). Whether they were merged is the only way to tell
        // a 4-frame minimum pause from a 5-frame one from the span alone. Five
        // is what Silero's reference needs: a quiet frame four frames after the
        // first quiet one.
        let gap = |n: usize| probs(&[(0.01, 4), (0.9, 5), (0.1, n), (0.9, 5), (0.01, 4)]);
        let merged = |n: usize| {
            let p = gap(n);
            speech_span(&p, p.len() as f64 * F).is_some()
        };
        assert!(
            merged(1) && merged(2) && merged(3) && merged(4),
            "a pause under 5 frames splits"
        );
        assert!(!merged(5), "a pause of 5 frames must end the speech");
        assert!(!merged(10));
    }

    #[test]
    fn scores_between_the_thresholds_neither_start_nor_end_speech() {
        // A long stretch at 0.4 -- above the end threshold, below the start one
        // -- keeps speech going, however long it lasts...
        let p = probs(&[(0.01, 4), (0.9, 6), (0.4, 20), (0.9, 6), (0.01, 4)]);
        let s = speech_span(&p, p.len() as f64 * F).unwrap();
        assert!(near(s.start, 4.0 * F) && near(s.end, 36.0 * F), "{s:?}");
        // ...and on its own it never starts any.
        assert_eq!(speech_span(&run(0.4, 40), 40.0 * F), None);
        // The end threshold is exclusive on the quiet side: 0.35 is not quiet.
        let p = probs(&[(0.01, 4), (0.9, 6), (0.35, 6), (0.9, 6), (0.01, 4)]);
        assert!(near(
            speech_span(&p, p.len() as f64 * F).unwrap().end,
            22.0 * F
        ));
    }

    #[test]
    fn a_quiet_tail_shorter_than_the_minimum_silence_is_not_speech() {
        let p = probs(&[(0.01, 4), (0.9, 12), (0.1, 4)]);
        let s = speech_span(&p, p.len() as f64 * F).unwrap();
        assert!(near(s.end, 16.0 * F), "end was {}", s.end);
    }

    #[test]
    fn only_a_short_burst_gives_none() {
        let p = probs(&[(0.01, 5), (0.95, 3), (0.01, 5)]);
        assert_eq!(speech_span(&p, 13.0 * F), None);
    }

    #[test]
    fn the_minimum_length_is_inclusive() {
        // ceil(0.25 / 0.032) = 8 frames.
        let p8 = probs(&[(0.01, 3), (0.9, 8), (0.01, 3)]);
        let p7 = probs(&[(0.01, 3), (0.9, 7), (0.01, 3)]);
        assert!(speech_span(&p8, 14.0 * F).is_some());
        assert!(speech_span(&p7, 13.0 * F).is_none());
    }

    #[test]
    fn hysteresis_one_weak_frame_does_not_split_speech() {
        // 0.4 sits between the thresholds: it neither starts nor ends speech.
        let p = probs(&[(0.01, 4), (0.9, 6), (0.4, 1), (0.9, 6), (0.01, 4)]);
        let s = speech_span(&p, 21.0 * F).unwrap();
        assert!(near(s.start, 4.0 * F));
        assert!(near(s.end, 17.0 * F));
    }

    #[test]
    fn hysteresis_a_middling_score_does_not_start_speech() {
        assert_eq!(speech_span(&run(0.45, 50), 50.0 * F), None);
    }

    #[test]
    fn speech_running_to_the_end_is_closed_at_the_end_of_the_file() {
        let p = probs(&[(0.01, 5), (0.9, 20)]);
        // The file is a hair shorter than the 25 whole frames.
        let total = 25.0 * F - 0.010;
        let s = speech_span(&p, total).unwrap();
        assert!(near(s.end, total), "end must be capped at the real length");
    }

    #[test]
    fn speech_from_the_first_frame_starts_at_zero() {
        let p = probs(&[(0.9, 20), (0.01, 5)]);
        assert!(near(speech_span(&p, 25.0 * F).unwrap().start, 0.0));
    }

    // ---- plan_trim --------------------------------------------------------

    #[test]
    fn plan_adds_the_margins_and_the_fades() {
        let plan = plan_trim(
            SpeechSpan {
                start: 2.0,
                end: 5.0,
            },
            10.0,
        );
        assert!(near(plan.start, 1.85));
        assert!(near(plan.end, 5.15));
        assert!(near(plan.fade_in, 0.075));
        assert!(near(plan.fade_out, 0.075));
    }

    #[test]
    fn plan_clamps_to_the_file_and_shrinks_the_fade_to_the_margin() {
        let plan = plan_trim(
            SpeechSpan {
                start: 0.04,
                end: 9.95,
            },
            10.0,
        );
        assert!(near(plan.start, 0.0));
        assert!(near(plan.end, 10.0));
        assert!(near(plan.fade_in, 0.04), "fade-in was {}", plan.fade_in);
        assert!(near(plan.fade_out, 0.05), "fade-out was {}", plan.fade_out);
    }

    #[test]
    fn plan_gives_no_fade_where_speech_touches_the_edge() {
        let plan = plan_trim(
            SpeechSpan {
                start: 0.0,
                end: 10.0,
            },
            10.0,
        );
        assert_eq!(plan.fade_in, 0.0);
        assert_eq!(plan.fade_out, 0.0);
    }

    #[test]
    fn fades_never_reach_into_the_speech() {
        for &(s, e, total) in &[
            (0.0, 0.3, 0.3),
            (0.01, 0.4, 0.45),
            (1.0, 1.25, 9.0),
            (0.149, 8.9, 9.0),
            (4.0, 4.5, 4.5),
        ] {
            let plan = plan_trim(SpeechSpan { start: s, end: e }, total);
            assert!(plan.start + plan.fade_in <= s + 1e-9, "{plan:?} vs {s}");
            assert!(plan.end - plan.fade_out >= e - 1e-9, "{plan:?} vs {e}");
            assert!(plan.start >= 0.0 && plan.end <= total + 1e-9);
        }
    }

    // ---- trim_filter ------------------------------------------------------

    #[test]
    fn filter_for_a_cut_on_both_sides() {
        let plan = plan_trim(
            SpeechSpan {
                start: 2.0,
                end: 5.0,
            },
            10.0,
        );
        assert_eq!(
            trim_filter(&plan, 10.0).unwrap(),
            "asetpts=PTS-STARTPTS,atrim=start=1.850:end=5.150,asetpts=PTS-STARTPTS,\
             afade=t=in:st=0:d=0.075,afade=t=out:st=3.225:d=0.075"
        );
    }

    #[test]
    fn filter_for_a_head_only_cut_has_no_end() {
        let plan = plan_trim(
            SpeechSpan {
                start: 2.0,
                end: 9.95,
            },
            10.0,
        );
        let f = trim_filter(&plan, 10.0).unwrap();
        assert!(f.contains("atrim=start=1.850,"), "{f}");
        assert!(!f.contains("end="), "{f}");
    }

    #[test]
    fn filter_for_a_tail_only_cut_has_no_start() {
        let plan = plan_trim(
            SpeechSpan {
                start: 0.05,
                end: 5.0,
            },
            10.0,
        );
        let f = trim_filter(&plan, 10.0).unwrap();
        assert!(f.contains("atrim=end=5.150,"), "{f}");
        assert!(!f.contains("start="), "{f}");
    }

    #[test]
    fn filter_skips_a_zero_length_fade() {
        let plan = plan_trim(
            SpeechSpan {
                start: 0.0,
                end: 5.0,
            },
            10.0,
        );
        let f = trim_filter(&plan, 10.0).unwrap();
        assert!(!f.contains("t=in"), "{f}");
        assert!(f.contains("t=out"), "{f}");
    }

    #[test]
    fn filter_cuts_even_a_few_milliseconds() {
        // Below a millisecond is rounding noise; 5 ms is a real cut.
        let plan = TrimPlan {
            start: 0.005,
            end: 4.0,
            fade_in: 0.0,
            fade_out: 0.0,
        };
        assert!(trim_filter(&plan, 4.0)
            .unwrap()
            .contains("atrim=start=0.005"));
        let noise = TrimPlan {
            start: 0.0004,
            ..plan
        };
        assert_eq!(trim_filter(&noise, 4.0), None);
    }

    #[test]
    fn filter_is_none_when_nothing_would_change() {
        let plan = plan_trim(
            SpeechSpan {
                start: 0.0,
                end: 10.0,
            },
            10.0,
        );
        assert_eq!(trim_filter(&plan, 10.0), None);
    }

    #[test]
    fn filter_is_one_line_without_whitespace() {
        let plan = plan_trim(
            SpeechSpan {
                start: 3.3,
                end: 7.7,
            },
            12.0,
        );
        let f = trim_filter(&plan, 12.0).unwrap();
        assert!(!f.chars().any(char::is_whitespace), "{f:?}");
    }

    #[test]
    fn fade_out_starts_inside_the_kept_audio() {
        // A kept length shorter than the fade must not produce a negative `st`.
        let plan = TrimPlan {
            start: 1.0,
            end: 1.04,
            fade_in: 0.0,
            fade_out: 0.075,
        };
        let f = trim_filter(&plan, 5.0).unwrap();
        assert!(f.contains("afade=t=out:st=0.000:"), "{f}");
    }

    // ---- model_path -------------------------------------------------------

    #[test]
    fn model_lookup_order() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        std::fs::create_dir_all(exe.join("models")).unwrap();
        let in_models = exe.join("models").join(MODEL_FILE);
        let beside = exe.join(MODEL_FILE);
        let env_file = dir.path().join("elsewhere.onnx");

        assert_eq!(model_path_in(None, Some(&exe)), None);

        std::fs::write(&beside, b"x").unwrap();
        assert_eq!(model_path_in(None, Some(&exe)), Some(beside.clone()));

        std::fs::write(&in_models, b"x").unwrap();
        assert_eq!(
            model_path_in(None, Some(&exe)),
            Some(in_models.clone()),
            "models/ wins over the exe directory itself"
        );

        std::fs::write(&env_file, b"x").unwrap();
        assert_eq!(
            model_path_in(Some(env_file.clone().into_os_string()), Some(&exe)),
            Some(env_file),
            "the environment variable wins over both"
        );
    }

    #[test]
    fn a_dangling_env_var_falls_through_to_the_default_location() {
        let dir = tempfile::tempdir().unwrap();
        let in_models = dir.path().join("models").join(MODEL_FILE);
        std::fs::create_dir_all(in_models.parent().unwrap()).unwrap();
        std::fs::write(&in_models, b"x").unwrap();
        let missing = dir.path().join("typo.onnx");
        assert_eq!(
            model_path_in(Some(missing.into_os_string()), Some(dir.path())),
            Some(in_models)
        );
        assert_eq!(
            model_path_in(Some("".into()), Some(dir.path())),
            Some(dir.path().join("models").join(MODEL_FILE))
        );
    }

    #[test]
    fn model_pin_is_well_formed() {
        assert_eq!(MODEL_SHA256.len(), 64);
        assert!(MODEL_SHA256
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(MODEL_URL.starts_with("https://"));
        assert!(
            MODEL_URL.contains("/v"),
            "the URL must name a release tag, not a branch"
        );
    }

    // ---- the quiet-file rescue --------------------------------------------

    #[test]
    fn rescue_gain_brings_the_level_to_the_target() {
        // 0.003 -> 0.3 is a factor of 100, i.e. exactly 40 dB; 0.03 -> 0.3 is
        // 10x, 20 dB; 0.06 -> 0.3 is 5x, 13.98 dB.
        let db = |level: f32| rescue_gain_db(level).unwrap();
        assert!((db(0.03) - 20.0).abs() < 1e-3, "{}", db(0.03));
        assert!((db(0.06) - 13.979).abs() < 1e-2, "{}", db(0.06));
        assert!((db(0.003) - 40.0).abs() < 1e-3, "{}", db(0.003));
        // The applied gain really does land the level on the target.
        let level = 0.012_f32;
        let landed = level * 10f32.powf(db(level) / 20.0);
        assert!((landed - QUIET_TARGET_LEVEL).abs() < 1e-4, "{landed}");
    }

    #[test]
    fn rescue_gain_is_capped() {
        assert_eq!(rescue_gain_db(0.0001), Some(QUIET_MAX_GAIN_DB));
        assert_eq!(rescue_gain_db(1e-20), Some(QUIET_MAX_GAIN_DB));
        assert_eq!(QUIET_MAX_GAIN_DB, 40.0);
    }

    #[test]
    fn rescue_is_off_for_loud_files_silence_and_nonsense() {
        assert_eq!(
            rescue_gain_db(QUIET_LEVEL),
            None,
            "the threshold is exclusive"
        );
        assert_eq!(rescue_gain_db(0.5), None);
        assert_eq!(rescue_gain_db(1.0), None);
        assert_eq!(rescue_gain_db(0.0), None, "digital silence gains nothing");
        assert_eq!(rescue_gain_db(-0.1), None);
        assert_eq!(rescue_gain_db(f32::NAN), None);
        assert_eq!(rescue_gain_db(f32::INFINITY), None);
        assert!(rescue_gain_db(QUIET_LEVEL * 0.99).is_some());
    }

    #[test]
    fn the_level_ignores_a_single_loud_frame() {
        // 500 frames of a quiet take and one bump of the microphone. The
        // largest-sample rule read this file as loud and skipped the rescue.
        let mut peaks = vec![0.005f32; 500];
        peaks[200] = 0.9;
        let level = level_of(&peaks);
        assert!((level - 0.005).abs() < 1e-6, "level {level}");
        assert!(rescue_gain_db(level).is_some());
    }

    #[test]
    fn the_level_still_sees_a_take_that_is_loud_throughout() {
        let peaks = vec![0.4f32; 300];
        assert!((level_of(&peaks) - 0.4).abs() < 1e-6);
        assert_eq!(rescue_gain_db(level_of(&peaks)), None);
    }

    #[test]
    fn a_take_with_a_few_percent_loud_frames_counts_as_loud() {
        // 5 of 100 frames carry the speech: the 99th percentile sees them, a
        // lower percentile would call this file quiet and boost it.
        let mut peaks = vec![0.01f32; 95];
        peaks.extend([0.8f32; 5]);
        assert!(
            (level_of(&peaks) - 0.8).abs() < 1e-6,
            "{}",
            level_of(&peaks)
        );
    }

    #[test]
    fn the_documented_numbers_are_the_ones_in_the_code() {
        // README.MD and the UI hint quote these. Changing one means changing the
        // text, and this is what makes that a deliberate act.
        assert_eq!(START_THRESHOLD, 0.5);
        assert_eq!(END_THRESHOLD, 0.35);
        assert_eq!(MIN_SPEECH_SECS, 0.25);
        assert_eq!(MARGIN_SECS, 0.150);
        assert_eq!(FADE_SECS, 0.075);
        assert_eq!(QUIET_LEVEL, 0.1); // -20 dBFS
        assert_eq!(QUIET_TARGET_LEVEL, 0.3); // about -10 dBFS
        assert_eq!(QUIET_MAX_GAIN_DB, 40.0);
        assert_eq!((WINDOW, SAMPLE_RATE), (512, 16_000));
    }

    #[test]
    fn the_start_threshold_is_inclusive_and_the_end_threshold_is_exclusive() {
        let span = |p: Vec<f32>| speech_span(&p, p.len() as f64 * F);
        // 0.5 starts speech; just under it does not.
        assert!(span(probs(&[(0.01, 3), (START_THRESHOLD, 12), (0.01, 6)])).is_some());
        assert!(span(probs(&[
            (0.01, 3),
            (START_THRESHOLD - 0.001, 12),
            (0.01, 6)
        ]))
        .is_none());
        // A score of exactly 0.35 is not quiet (the pause never gets going, so
        // the speech runs on); just under it is.
        let with_gap = |p: f32| probs(&[(0.01, 3), (0.9, 5), (p, 6), (0.9, 5), (0.01, 6)]);
        assert!(near(
            speech_span(&with_gap(END_THRESHOLD), 25.0 * F).unwrap().end,
            19.0 * F
        ));
        assert!(
            speech_span(&with_gap(END_THRESHOLD - 0.001), 25.0 * F).is_none(),
            "5 + 6 + 5 frames: a 6-frame pause under 0.35 splits two bursts too short to count"
        );
    }

    #[test]
    fn the_level_of_nothing_is_zero_and_of_one_frame_is_that_frame() {
        assert_eq!(level_of(&[]), 0.0);
        assert_eq!(level_of(&[0.07]), 0.07);
        // The top 1% is dropped: 100 frames, the loudest one is ignored.
        let mut peaks = vec![0.01f32; 99];
        peaks.push(0.8);
        assert!((level_of(&peaks) - 0.01).abs() < 1e-6);
    }

    // ---- the model --------------------------------------------------------

    /// The model's own output on real speech, pinned.
    ///
    /// The boundary tests in `tests/vad_integration.rs` tolerate two frames, so
    /// they would not notice the model being fed slightly wrong input -- a
    /// missing context window, a state that is not carried over -- as long as
    /// the scores stay on the right side of the thresholds. These do. Values
    /// were measured on the first 10 s of Silero's `tests/data/test.wav` with
    /// `silero_vad.onnx` v6.2.2; the tolerance is wide enough for CPU-to-CPU
    /// floating-point differences and far too narrow for a wrong input.
    ///
    /// `#[ignore]` because it needs the model and the fixture
    /// (`VOCAN_SILERO_MODEL`, `VOCAN_VAD_FIXTURE`); skips without them.
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    #[test]
    #[ignore]
    fn probabilities_match_the_reference_run() {
        let (Some(model), Some(fixture)) = (
            model_path(),
            std::env::var_os("VOCAN_VAD_FIXTURE").map(PathBuf::from),
        ) else {
            // CI sets VOCAN_REQUIRE_VAD: there, a skip would be a green no-op.
            let required = std::env::var_os("VOCAN_REQUIRE_VAD").is_some_and(|v| !v.is_empty());
            assert!(
                !required,
                "VOCAN_REQUIRE_VAD is set but VOCAN_SILERO_MODEL / VOCAN_VAD_FIXTURE do not \
                 point at existing files"
            );
            eprintln!("SKIP: needs VOCAN_SILERO_MODEL and VOCAN_VAD_FIXTURE");
            return;
        };
        let mut reader = hound::WavReader::open(fixture).unwrap();
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .take(SAMPLE_RATE as usize * 10)
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();

        let mut silero = engine::Silero::load(&model).unwrap();
        let mut probs = Vec::new();
        for frame in samples.chunks_exact(WINDOW) {
            probs.push(silero.probability(frame).unwrap());
        }

        let sampled: Vec<f32> = REFERENCE_FRAMES.iter().map(|&i| probs[i]).collect();
        for (&i, (&got, &want)) in REFERENCE_FRAMES
            .iter()
            .zip(sampled.iter().zip(REFERENCE_PROBS.iter()))
        {
            assert!(
                (got - want).abs() < 0.02,
                "frame {i}: model gave {got}, reference {want}"
            );
        }
        let sum: f32 = probs.iter().sum();
        assert!(
            (sum - REFERENCE_SUM).abs() < 0.5,
            "sum of 10 s of scores {sum}, reference {REFERENCE_SUM}"
        );
    }

    /// Frames of the 10 s excerpt that are sampled: speech onset, mid-word,
    /// the first pause, the second phrase, and the long pause after 6.8 s.
    const REFERENCE_FRAMES: [usize; 12] = [0, 1, 2, 20, 61, 63, 70, 85, 140, 160, 215, 250];
    const REFERENCE_PROBS: [f32; 12] = [
        0.2083, 0.8179, 0.8912, 0.9999, 0.9339, 0.3287, 0.0183, 0.9974, 0.9998, 1.0000, 0.0358,
        0.0318,
    ];
    const REFERENCE_SUM: f32 = 203.92;

    // ---- for_each_frame ---------------------------------------------------

    /// A reader that hands out at most `n` bytes per call, to land reads off
    /// the 4-byte sample boundary.
    struct Dribble<'a> {
        data: &'a [u8],
        n: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let take = self.n.min(buf.len()).min(self.data.len());
            buf[..take].copy_from_slice(&self.data[..take]);
            self.data = &self.data[take..];
            Ok(take)
        }
    }

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn frames_survive_reads_that_split_a_sample() {
        let samples: Vec<f32> = (0..WINDOW * 2 + 100).map(|i| i as f32).collect();
        let data = bytes(&samples);
        for n in [1usize, 3, 5, 7, 4096] {
            let mut got: Vec<Vec<f32>> = Vec::new();
            let total = for_each_frame(&mut Dribble { data: &data, n }, |f| {
                got.push(f.to_vec());
                Ok(())
            })
            .unwrap();
            assert_eq!(total, samples.len() as u64, "n={n}");
            assert_eq!(got.len(), 3, "n={n}");
            assert_eq!(&got[0][..], &samples[..WINDOW], "n={n}");
            assert_eq!(&got[1][..], &samples[WINDOW..2 * WINDOW], "n={n}");
            // The last frame holds 100 real samples, then zeros.
            assert_eq!(&got[2][..100], &samples[2 * WINDOW..], "n={n}");
            assert!(got[2][100..].iter().all(|&s| s == 0.0), "n={n}");
        }
    }

    #[test]
    fn an_exact_multiple_of_the_window_has_no_padded_frame() {
        let data = bytes(&vec![0.5f32; WINDOW * 3]);
        let mut frames = 0;
        let total = for_each_frame(&mut Cursor::new(data), |_| {
            frames += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!((frames, total), (3, (WINDOW * 3) as u64));
    }

    #[test]
    fn empty_input_is_zero_samples() {
        let mut frames = 0;
        let total = for_each_frame(&mut Cursor::new(Vec::new()), |_| {
            frames += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!((frames, total), (0, 0));
    }

    #[test]
    fn an_error_from_the_callback_stops_the_stream() {
        let data = bytes(&vec![0.0f32; WINDOW * 5]);
        let mut calls = 0;
        let r = for_each_frame(&mut Cursor::new(data), |_| {
            calls += 1;
            Err(anyhow!("boom"))
        });
        assert!(r.is_err());
        assert_eq!(calls, 1);
    }
}
