#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::proc::output_supervised;
use crate::types::LoudnormStats;

std::thread_local! {
    /// How many ffmpeg invocations this thread has built.
    ///
    /// Process startup dominates the cost of a short voice-over file -- a
    /// typical line is converted in less time than it takes to launch the four
    /// processes involved -- so the per-file process count is a performance
    /// budget, not an implementation detail. `tests/spawn_budget.rs` asserts it.
    ///
    /// Two deliberate choices:
    ///
    /// * **Unconditional, not `#[cfg(test)]`.** Integration tests under
    ///   `tests/` link `vocan` compiled *without* `cfg(test)`, so a counter
    ///   behind that attribute would not exist in the code being measured, and
    ///   the assertion would silently be checking nothing. One non-atomic
    ///   increment against a `Command::spawn` is unmeasurable.
    /// * **Thread-local, not a global atomic.** `cargo test` runs test
    ///   functions on separate threads of one binary, and several ffmpeg tests
    ///   spawn concurrently; a shared counter would total them up and make any
    ///   assertion flaky -- worse than no assertion, because it would look like
    ///   coverage.
    ///
    /// Every construction here is followed by a spawn, so this counts real
    /// processes. If that ever stops being true the budget test's numbers
    /// change, which is the alarm doing its job.
    static SPAWN_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Number of ffmpeg invocations built on the current thread since the last
/// [`reset_spawn_count`]. See [`struct@SPAWN_COUNT`].
pub fn spawn_count() -> u64 {
    SPAWN_COUNT.with(|c| c.get())
}

/// Zeroes the current thread's counter, returning its previous value.
pub fn reset_spawn_count() -> u64 {
    SPAWN_COUNT.with(|c| c.replace(0))
}

pub fn ffmpeg_cmd(ffmpeg: &Path) -> Command {
    SPAWN_COUNT.with(|c| c.set(c.get() + 1));

    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    // Nothing here ever feeds ffmpeg on stdin, and an inherited stdin lets it
    // consume input meant for the parent process.
    cmd.arg("-nostdin");
    cmd
}

/// Returns `true` for file extensions that ffmpeg can decode as audio.
pub fn is_audio_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "wav"
                | "wave"
                | "mp3"
                | "flac"
                | "aiff"
                | "aif"
                | "ogg"
                | "opus"
                | "m4a"
                | "aac"
                | "wma"
                | "mp2"
                | "ac3"
                | "dts"
                | "mka"
        )
    )
}

/// Priority:
///   1. `ffmpeg` available on PATH (verified by running `ffmpeg -version`)
///   2. `ffmpeg.exe` / `ffmpeg` sitting next to the current executable
pub fn find_ffmpeg() -> Result<PathBuf> {
    /// A `Command` that will not flash a console window on Windows.
    ///
    /// The probes below used to build `Command::new` directly, bypassing
    /// `ffmpeg_cmd` and therefore missing `CREATE_NO_WINDOW` -- so launching
    /// VOCAN briefly popped up one or two console windows every single time.
    /// `ffmpeg_cmd` itself is not reused here because it appends `-nostdin`,
    /// which a bare `-version` probe has no reason to depend on.
    fn silent_probe(bin: &Path) -> Command {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut cmd = Command::new(bin);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd
    }

    let path_probe = silent_probe(Path::new("ffmpeg"))
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if path_probe.map(|s| s.success()).unwrap_or(false) {
        return Ok(PathBuf::from("ffmpeg"));
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let candidate = exe_dir.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            if candidate.is_file() {
                let local_probe = silent_probe(&candidate)
                    .arg("-version")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if local_probe.map(|s| s.success()).unwrap_or(false) {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(anyhow!(
        "FFmpeg not found.\n\
         Place ffmpeg.exe in the same folder as this application, \
         or add it to your system PATH."
    ))
}

/// Runs `ffmpeg -i <input>` once and returns its stderr, which is where FFmpeg
/// prints the stream summary (sample rate, duration) when no output is given.
fn probe_stderr(input: &Path, ffmpeg: &Path) -> Option<String> {
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-hide_banner", "-i"]).arg(input);
    let output = output_supervised(&mut cmd).ok()?;
    Some(String::from_utf8_lossy(&output.stderr).into_owned())
}

/// Extracts the sample rate from a probe's stderr text.
fn parse_sample_rate(stderr: &str) -> Option<u32> {
    let tokens: Vec<&str> = stderr.split_whitespace().collect();
    for window in tokens.windows(2) {
        // Real ffmpeg output is "<rate> Hz, <channels> ..." -- "Hz" carries
        // a trailing comma, so it must be trimmed just like the number is.
        if window[1].trim_end_matches(',') == "Hz" {
            if let Ok(sr) = window[0].trim_end_matches(',').parse::<u32>() {
                if (8_000..=192_000).contains(&sr) {
                    return Some(sr);
                }
            }
        }
    }
    None
}

/// Extracts the duration in seconds from a probe's stderr text.
fn parse_duration(stderr: &str) -> Option<f32> {
    for line in stderr.lines() {
        if line.contains("Duration:") {
            for token in line.split_whitespace() {
                let t = token.trim_end_matches(',');
                let parts: Vec<&str> = t.split(':').collect();
                if parts.len() == 3 {
                    if let (Ok(h), Ok(m), Ok(s)) = (
                        parts[0].parse::<f32>(),
                        parts[1].parse::<f32>(),
                        parts[2].parse::<f32>(),
                    ) {
                        return Some(h * 3600.0 + m * 60.0 + s);
                    }
                }
            }
        }
    }
    None
}

/// Returns both the sample rate and the duration from a **single** ffmpeg
/// process.
///
/// Callers that need both (the non-Automixer path in `process_single_file`)
/// should use this rather than calling [`get_sample_rate`] and
/// [`get_duration`] back to back: those spawn one process each and then parse
/// the very same stderr text, doubling the process count for every file in a
/// batch.
pub fn probe_input(input: &Path, ffmpeg: &Path) -> (Option<u32>, Option<f32>) {
    match probe_stderr(input, ffmpeg) {
        Some(stderr) => (parse_sample_rate(&stderr), parse_duration(&stderr)),
        None => (None, None),
    }
}

/// Returns the sample rate of the source file.
pub fn get_sample_rate(input: &Path, ffmpeg: &Path) -> Option<u32> {
    parse_sample_rate(&probe_stderr(input, ffmpeg)?)
}

/// Returns the duration of the file in seconds.
pub fn get_duration(input: &Path, ffmpeg: &Path) -> Option<f32> {
    parse_duration(&probe_stderr(input, ffmpeg)?)
}

/// Builds an error carrying FFmpeg's own explanation of a failed run.
///
/// The measurement helpers below used to ignore the exit status entirely and go
/// straight to parsing, so a file FFmpeg refused outright came back as
/// "Missing JSON in FFmpeg output" -- a description of our parser, not of the
/// problem. FFmpeg prints the actual reason last, after the banner and the
/// stream dump, so the tail is the part worth keeping.
pub(crate) fn ffmpeg_failed(stage: &str, stderr: &str) -> anyhow::Error {
    const MAX_TAIL_CHARS: usize = 600;
    let trimmed = stderr.trim_end();
    let tail = match trimmed.char_indices().nth_back(MAX_TAIL_CHARS - 1) {
        Some((start, _)) => format!("...{}", &trimmed[start..]),
        None => trimmed.to_string(),
    };
    if tail.is_empty() {
        anyhow!("FFmpeg failed during {stage} without reporting a reason")
    } else {
        anyhow!("FFmpeg failed during {stage}: {tail}")
    }
}

/// Measures the integrated loudness (LUFS-I) for folder overview analysis.
pub fn measure_lufs(input: &Path, ffmpeg: &Path) -> Result<Option<f32>> {
    let filter = "loudnorm=I=-23:TP=-1.5:LRA=1.0:print_format=json";
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", filter, "-f", "null", "-"]);
    let output = output_supervised(&mut cmd).context("FFmpeg error during loudness measurement")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(ffmpeg_failed("loudness measurement", &stderr_str));
    }
    let stats = extract_loudnorm_stats(&stderr_str)?;

    match stats.input_i.parse::<f32>() {
        Ok(val) if val.is_finite() && val >= -99.0 => Ok(Some(val)),
        _ => Ok(None),
    }
}

/// Normalization Pass 1 (standard, no padding).
pub fn get_file_stats(
    input: &Path,
    ffmpeg: &Path,
    target_lufs: f32,
    prefix: Option<&str>,
) -> Result<Option<LoudnormStats>> {
    let loudnorm = format!(
        "loudnorm=I={target}:TP=-1.5:LRA=1.0:print_format=json",
        target = target_lufs
    );
    let filter = match prefix {
        Some(p) => format!("{},{}", p, loudnorm),
        None => loudnorm,
    };
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"]);
    let output =
        output_supervised(&mut cmd).context("FFmpeg error during loudnorm analysis (pass 1)")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(ffmpeg_failed("loudnorm analysis (pass 1)", &stderr_str));
    }
    let stats = extract_loudnorm_stats(&stderr_str)?;

    match stats.input_i.parse::<f32>() {
        Ok(val) if val.is_finite() && val >= -99.0 => Ok(Some(stats)),
        _ => Ok(None),
    }
}

/// Normalization Pass 1 with silence padding — for files ~1-3s.
pub fn get_file_stats_padded(
    input: &Path,
    ffmpeg: &Path,
    target_lufs: f32,
    pad_to_secs: f32,
    prefix: Option<&str>,
) -> Result<Option<LoudnormStats>> {
    let loudnorm = format!(
        "loudnorm=I={target}:TP=-1.5:LRA=1.0:print_format=json",
        target = target_lufs
    );
    let pad_chain = format!(
        "apad=pad_dur={pad},atrim=end={pad},{loudnorm}",
        pad = pad_to_secs,
        loudnorm = loudnorm,
    );
    let filter = match prefix {
        Some(p) => format!("{},{}", p, pad_chain),
        None => pad_chain,
    };
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"]);
    let output = output_supervised(&mut cmd)
        .context("FFmpeg error during padded loudnorm analysis (pass 1)")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(ffmpeg_failed(
            "padded loudnorm analysis (pass 1)",
            &stderr_str,
        ));
    }
    let stats = extract_loudnorm_stats(&stderr_str)?;

    match stats.input_i.parse::<f32>() {
        Ok(val) if val.is_finite() && val >= -99.0 => Ok(Some(stats)),
        _ => Ok(None),
    }
}

/// Measures the file's peak level (dBFS) using the `volumedetect` filter.
pub fn measure_peak_dbfs(input: &Path, ffmpeg: &Path, prefix: Option<&str>) -> Result<f32> {
    let filter = match prefix {
        Some(p) => format!("{},volumedetect", p),
        None => "volumedetect".to_string(),
    };
    let mut cmd = ffmpeg_cmd(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"]);
    let output = output_supervised(&mut cmd).context("FFmpeg error during peak measurement")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(ffmpeg_failed("peak measurement", &stderr_str));
    }

    for line in stderr_str.lines() {
        if line.contains("max_volume:") {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            for (i, token) in tokens.iter().enumerate() {
                if *token == "max_volume:" {
                    if let Some(val_str) = tokens.get(i + 1) {
                        if let Ok(val) = val_str.parse::<f32>() {
                            return Ok(val);
                        }
                    }
                }
            }
        }
    }

    Err(anyhow!("Could not parse max_volume from FFmpeg output"))
}

pub fn apply_loudnorm_pass2(
    cmd: &mut Command,
    target_lufs: f32,
    stats: &LoudnormStats,
    source_sr: u32,
    prefix: Option<&str>,
) {
    let loudnorm = format!(
        "loudnorm=I={lufs}:TP=-1.5:LRA=1.0:\
         measured_I={mi}:measured_LRA={mlra}:measured_TP={mtp}:\
         measured_thresh={mt}:offset={off}:linear=true",
        lufs = target_lufs,
        mi = stats.input_i,
        mlra = stats.input_lra,
        mtp = stats.input_tp,
        mt = stats.input_thresh,
        off = stats.target_offset,
    );
    let filter = match prefix {
        Some(p) => format!("{},{}", p, loudnorm),
        None => loudnorm,
    };
    cmd.args(["-af", &filter, "-ar", &source_sr.to_string()]);
}

pub fn extract_loudnorm_stats(stderr: &str) -> Result<LoudnormStats> {
    let start_idx = stderr.rfind('{').context("Missing JSON in FFmpeg output")?;
    let end_idx = stderr.rfind('}').context("Missing JSON in FFmpeg output")?;
    // Each brace is located independently, so a stray '{' printed *after* the
    // JSON block (ffmpeg is free to keep writing to stderr) leaves start_idx
    // past end_idx. Slicing that range panics rather than failing the parse --
    // and the caller's `catch_unwind` would report it as an opaque
    // "Panic while processing" with no indication of the real cause.
    if start_idx >= end_idx {
        return Err(anyhow!("Malformed JSON block in FFmpeg output"));
    }
    let json_str = &stderr[start_idx..=end_idx];
    let stats: LoudnormStats = serde_json::from_str(json_str)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Representative capture of real `ffmpeg -af loudnorm=...:print_format=json`
    // stderr output (banner lines trimmed, JSON block kept verbatim in shape).
    const SAMPLE_STDERR: &str = r#"
Input #0, wav, from 'input.wav':
  Duration: 00:00:05.00, bitrate: 1411 kb/s
Stream #0:0: Audio: pcm_s16le, 44100 Hz, mono, s16, 705 kb/s
[Parsed_loudnorm_0 @ 0x7f8b3b008dc0]
{
	"input_i" : "-23.50",
	"input_tp" : "-6.00",
	"input_lra" : "5.00",
	"input_thresh" : "-33.50",
	"output_i" : "-23.00",
	"output_tp" : "-2.00",
	"output_lra" : "5.00",
	"output_thresh" : "-33.00",
	"normalization_type" : "dynamic",
	"target_offset" : "0.50"
}
size=N/A time=00:00:05.00 bitrate=N/A speed=  42x
"#;

    #[test]
    fn extract_loudnorm_stats_parses_trailing_json() {
        let stats = extract_loudnorm_stats(SAMPLE_STDERR).unwrap();
        assert_eq!(stats.input_i, "-23.50");
        assert_eq!(stats.input_tp, "-6.00");
        assert_eq!(stats.input_lra, "5.00");
        assert_eq!(stats.input_thresh, "-33.50");
        assert_eq!(stats.target_offset, "0.50");
    }

    #[test]
    fn extract_loudnorm_stats_missing_json_errors() {
        let stderr = "Input #0, wav, from 'input.wav':\nno json here at all\n";
        assert!(extract_loudnorm_stats(stderr).is_err());
    }

    #[test]
    fn extract_loudnorm_stats_malformed_json_errors() {
        let stderr = "[Parsed_loudnorm_0]\n{ this is not valid json }\n";
        assert!(extract_loudnorm_stats(stderr).is_err());
    }

    #[test]
    fn extract_loudnorm_stats_only_opening_brace_errors() {
        let stderr = "[Parsed_loudnorm_0]\n{ \"input_i\": \"-23.50\" \n";
        assert!(extract_loudnorm_stats(stderr).is_err());
    }

    #[test]
    fn extract_loudnorm_stats_trailing_open_brace_after_json_errors_without_panicking() {
        // Regression test: `rfind('{')` and `rfind('}')` are independent, so a
        // '{' emitted after the JSON block puts start_idx past end_idx. The
        // slice `&stderr[start_idx..=end_idx]` used to panic here instead of
        // returning Err.
        let stderr = "{ \"input_i\" : \"-23.50\" }\nffmpeg: unexpected {\n";
        assert!(extract_loudnorm_stats(stderr).is_err());
    }

    #[test]
    fn ffmpeg_failed_keeps_the_tail_where_the_real_reason_lives() {
        // FFmpeg prints the banner and stream dump first and the actual error
        // last, so truncation has to keep the end, not the beginning.
        let stderr = format!(
            "{}\nInvalid data found when processing input\n",
            "x".repeat(5000)
        );
        let err = ffmpeg_failed("conversion", &stderr).to_string();
        assert!(err.contains("Invalid data found"), "got: {err}");
        assert!(err.contains("conversion"));
        assert!(
            err.len() < 1000,
            "the tail should be bounded, got {}",
            err.len()
        );
    }

    #[test]
    fn ffmpeg_failed_truncates_on_a_char_boundary() {
        // Slicing a multi-byte codepoint in half would panic. Each 'ą' is two
        // bytes, so a byte-offset cut would land inside one.
        let stderr = "ą".repeat(5000);
        let err = ffmpeg_failed("conversion", &stderr).to_string();
        assert!(err.contains('ą'));
    }

    #[test]
    fn ffmpeg_failed_reports_something_useful_when_stderr_is_empty() {
        let err = ffmpeg_failed("conversion", "   \n").to_string();
        assert!(err.contains("without reporting a reason"), "got: {err}");
    }

    #[test]
    fn extract_loudnorm_stats_braces_in_reverse_order_errors_without_panicking() {
        // The minimal shape of the same bug: a '}' that precedes the last '{'.
        let stderr = "closing } first, opening { last";
        assert!(extract_loudnorm_stats(stderr).is_err());
    }
}
