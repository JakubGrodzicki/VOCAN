//! Shared helpers for ffmpeg-dependent integration tests.
//!
//! These generate small synthetic WAV fixtures on the fly via `hound`
//! (already a project dependency) instead of checking in binary audio
//! assets into the repo.

use std::path::Path;

pub fn write_sine_wav(
    path: &Path,
    duration_secs: f32,
    sample_rate: u32,
    freq_hz: f32,
    amplitude: f32,
) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    let n = (sample_rate as f32 * duration_secs) as usize;
    for i in 0..n {
        let t = i as f32 / sample_rate as f32;
        let s = amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin();
        writer.write_sample(s).expect("write sample");
    }
    writer.finalize().expect("finalize wav");
}

#[allow(dead_code)]
pub fn write_silence_wav(path: &Path, duration_secs: f32, sample_rate: u32) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    let n = (sample_rate as f32 * duration_secs) as usize;
    for _ in 0..n {
        writer.write_sample(0.0f32).expect("write sample");
    }
    writer.finalize().expect("finalize wav");
}

/// Resolves ffmpeg the same way the app does at runtime: PATH first.
///
/// `#[allow(dead_code)]` throughout this module: `tests/common/mod.rs` is
/// recompiled fresh per integration-test binary (`tests/*.rs`), and each
/// binary only uses a subset of these helpers -- without the allow, `-D
/// warnings` would fail the build over a function a *different* test file
/// happens to use.
#[allow(dead_code)]
pub fn ffmpeg_path() -> std::path::PathBuf {
    std::path::PathBuf::from("ffmpeg")
}

/// Returns `true` if a real `ffmpeg` binary is reachable on PATH.
///
/// Every ffmpeg-dependent test checks this and skips (rather than fails)
/// when it's `false`, so `cargo test -- --ignored` degrades gracefully on
/// a machine without ffmpeg installed, instead of hard-failing.
#[allow(dead_code)]
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Call at the top of every `#[ignore]`d ffmpeg-dependent test. Returns
/// `true` (meaning "skip this test") and prints a notice if ffmpeg isn't
/// available; returns `false` otherwise.
#[allow(dead_code)]
pub fn skip_if_no_ffmpeg() -> bool {
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not found on PATH");
        return true;
    }
    false
}

/// Checks whether the installed ffmpeg has a given encoder (e.g. "libvorbis",
/// "libmp3lame"). Codec availability varies by build/distribution -- a
/// minimal package-manager install may lack optional third-party encoders
/// that a full/static build would include. Tests use this to skip a specific
/// output format gracefully rather than asserting a codec set this
/// environment's ffmpeg doesn't actually have.
#[allow(dead_code)]
pub fn ffmpeg_has_encoder(name: &str) -> bool {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(name)),
        Err(_) => false,
    }
}

/// Reads a WAV file back to mono f32 samples, downmixing if stereo.
/// Used by e2e tests to inspect actual output-file content (finiteness,
/// clipping) rather than just checking the file exists.
#[allow(dead_code)]
pub fn read_wav_samples_f32(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("open output wav");
    let spec = reader.spec();

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
        hound::SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / max_val)
                .collect()
        }
    };

    if spec.channels == 1 {
        samples
    } else {
        let ch = spec.channels as usize;
        let frames = samples.len() / ch;
        (0..frames)
            .map(|i| {
                let start = i * ch;
                samples[start..start + ch].iter().sum::<f32>() / ch as f32
            })
            .collect()
    }
}

/// Searches `PATH` for an executable with this name (adding `.exe` on
/// Windows), the same way a shell would resolve a bare command name.
/// `std` has no built-in `which`, so this is a small manual implementation.
#[allow(dead_code)]
pub fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(&exe_name))
        .find(|candidate| candidate.is_file())
}

/// Finds a `deep-filter` (DeepFilterNet3) binary sitting in the *same*
/// directory as a `ffmpeg` binary that's also on PATH.
///
/// This mirrors the layout `installMacLinux.sh` / `installWindows.ps1`
/// produce (a copy of `deep-filter` next to the resolved ffmpeg binary) and,
/// more importantly, the layout `process_with_rust_dsp` actually requires:
/// its own `deep-filter` lookup is `ffmpeg.parent()` (falling back to "next
/// to the running executable"), which is empty when `ffmpeg` is only a bare
/// PATH-relative name -- so tests must pass the *resolved* ffmpeg path for
/// that lookup to succeed, which is exactly what this helper returns.
///
/// Returns `None` (meaning "skip this test") if either binary isn't
/// findable, or if `deep-filter` isn't in ffmpeg's own directory.
#[allow(dead_code)]
pub fn deep_filter_next_to_ffmpeg() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let ffmpeg = find_on_path("ffmpeg")?;
    let dir = ffmpeg.parent()?;
    let dfn_name = if cfg!(windows) {
        "deep-filter.exe"
    } else {
        "deep-filter"
    };
    let dfn_bin = dir.join(dfn_name);
    if dfn_bin.is_file() {
        Some((ffmpeg, dfn_bin))
    } else {
        None
    }
}
