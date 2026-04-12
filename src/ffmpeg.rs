#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::types::LoudnormStats;

pub fn ffmpeg_cmd(ffmpeg: &Path) -> Command {
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
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
            "wav" | "wave" | "mp3" | "flac" | "aiff" | "aif" | "ogg" | "opus"
                | "m4a" | "aac" | "wma" | "mp2" | "ac3" | "dts" | "mka"
        )
    )
}

/// Priority:
///   1. `ffmpeg` available on PATH (verified by running `ffmpeg -version`)
///   2. `ffmpeg.exe` / `ffmpeg` sitting next to the current executable
pub fn find_ffmpeg() -> Result<PathBuf> {
    let path_probe = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if path_probe.map(|s| s.success()).unwrap_or(false) {
        return Ok(PathBuf::from("ffmpeg"));
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let candidate =
                exe_dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
            if candidate.is_file() {
                let local_probe = Command::new(&candidate)
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

/// Returns the sample rate of the source file.
pub fn get_sample_rate(input: &Path, ffmpeg: &Path) -> Option<u32> {
    let output = ffmpeg_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(input)
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let tokens: Vec<&str> = stderr.split_whitespace().collect();
    for window in tokens.windows(2) {
        if window[1] == "Hz" {
            if let Ok(sr) = window[0].trim_end_matches(',').parse::<u32>() {
                if (8_000..=192_000).contains(&sr) {
                    return Some(sr);
                }
            }
        }
    }
    None
}

/// Returns the duration of the file in seconds.
pub fn get_duration(input: &Path, ffmpeg: &Path) -> Option<f32> {
    let output = ffmpeg_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(input)
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
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

/// Measures the integrated loudness (LUFS-I) for folder overview analysis.
pub fn measure_lufs(input: &Path, ffmpeg: &Path) -> Result<Option<f32>> {
    let filter = "loudnorm=I=-23:TP=-1.5:LRA=1.0:print_format=json";
    let output = ffmpeg_cmd(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", filter, "-f", "null", "-"])
        .stderr(Stdio::piped())
        .output()
        .context("FFmpeg error during loudness measurement")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
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
    let output = ffmpeg_cmd(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"])
        .stderr(Stdio::piped())
        .output()
        .context("FFmpeg error during loudnorm analysis (pass 1)")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
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
    let output = ffmpeg_cmd(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"])
        .stderr(Stdio::piped())
        .output()
        .context("FFmpeg error during padded loudnorm analysis (pass 1)")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
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
    let output = ffmpeg_cmd(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"])
        .stderr(Stdio::piped())
        .output()
        .context("FFmpeg error during peak measurement")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);

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
    let json_str = &stderr[start_idx..=end_idx];
    let stats: LoudnormStats = serde_json::from_str(json_str)?;
    Ok(stats)
}
