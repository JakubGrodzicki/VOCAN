#![cfg_attr(windows, windows_subsystem = "windows")]
use anyhow::{anyhow, Context, Result};
use eframe::egui;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc::{self, Receiver, Sender}};
use std::thread;
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Data Types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct LoudnormStats {
    input_i: String,
    input_tp: String,
    input_lra: String,
    input_thresh: String,
    target_offset: String,
}

/// Describes the normalization method used for a file — used for logging.
enum NormResult {
    /// Standard 2-pass EBU R128 (files >= ~3s).
    Standard,
    /// 2-pass EBU R128 with silence padding (files ~1-3s, returning -inf without padding).
    Padded,
    /// Peak normalization (files < 1s, too short for EBU R128 integration).
    Peak { gain_db: f32 },
    /// Conversion without normalization (extreme fallback — silent or empty signal).
    Skipped,
}

enum AppMsg {
    Log(String),
    Progress(usize, usize),
    Error(String),
    Finished,
    Stopped,
    AnalysisResult(f32),
}

// ---------------------------------------------------------------------------
// Application State
// ---------------------------------------------------------------------------

struct AudioBatchApp {
    input_dir: String,
    output_dir: String,
    normalize_volume: bool,
    target_lufs: f32,
    /// Target peak (dBFS) used for files < 1s (peak normalization).
    target_peak_dbfs: f32,
    /// Applies a fixed processing chain (EQ → De-esser → Compressor) before normalization.
    automixer: bool,
    is_processing: bool,
    is_analyzing: bool,
    average_lufs: Option<f32>,
    logs: Vec<String>,
    current_progress: usize,
    total_files: usize,
    receiver: Receiver<AppMsg>,
    sender: Sender<AppMsg>,
    ffmpeg_path: PathBuf,
    /// Shared cancellation flag — set to `true` to request the worker thread to stop.
    cancel_flag: Arc<AtomicBool>,
}

impl AudioBatchApp {
    fn new(ffmpeg_path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            input_dir: String::new(),
            output_dir: String::new(),
            normalize_volume: false,
            target_lufs: -14.0,
            target_peak_dbfs: -3.0,
            automixer: false,
            is_processing: false,
            is_analyzing: false,
            average_lufs: None,
            logs: Vec::new(),
            current_progress: 0,
            total_files: 0,
            receiver,
            sender,
            ffmpeg_path,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_analysis(&mut self, ctx: egui::Context) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            let folder_path = path.to_path_buf();
            self.is_analyzing = true;
            self.average_lufs = None;
            self.cancel_flag.store(false, Ordering::Relaxed);
            self.logs
                .push(format!("Started folder analysis: {}", folder_path.display()));

            let tx = self.sender.clone();
            let ffmpeg_path = self.ffmpeg_path.clone();
            let cancel = Arc::clone(&self.cancel_flag);
            thread::spawn(move || {
                let files: Vec<PathBuf> = WalkDir::new(&folder_path)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file() && is_audio_file(e.path()))
                    .map(|e| e.path().to_path_buf())
                    .collect();

                let total = files.len();
                let mut sum_lufs = 0.0f32;
                let mut count = 0usize;

                for (i, file) in files.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        let _ = tx.send(AppMsg::Log("Analysis stopped by user.".into()));
                        let _ = tx.send(AppMsg::Stopped);
                        ctx.request_repaint();
                        return;
                    }

                    let _ = tx.send(AppMsg::Progress(i + 1, total));
                    ctx.request_repaint();

                    match measure_lufs(file, &ffmpeg_path) {
                        Ok(Some(val)) => {
                            sum_lufs += val;
                            count += 1;
                        }
                        Ok(None) => {
                            let _ = tx.send(AppMsg::Log(format!(
                                "Skipped in analysis (too short/quiet): {}",
                                file.display()
                            )));
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Log(format!(
                                "Analysis error for {}: {}",
                                file.display(),
                                e
                            )));
                        }
                    }
                }

                if count > 0 {
                    let avg = sum_lufs / count as f32;
                    let _ = tx.send(AppMsg::AnalysisResult(avg));
                    let _ = tx.send(AppMsg::Log(format!(
                        "Analysis finished. Average LUFS: {:.2} (from {} files)",
                        avg, count
                    )));
                } else {
                    let _ = tx.send(AppMsg::Error(
                        "No valid audio files found for analysis.".into(),
                    ));
                }
                let _ = tx.send(AppMsg::Finished);
                ctx.request_repaint();
            });
        }
    }

    fn start_processing(&mut self, ctx: egui::Context) {
        self.is_processing = true;
        self.logs.clear();
        self.current_progress = 0;
        self.total_files = 0;
        self.cancel_flag.store(false, Ordering::Relaxed);

        let input_path = PathBuf::from(&self.input_dir);
        let output_path = PathBuf::from(&self.output_dir);

        let lufs_option = if self.normalize_volume {
            Some(self.target_lufs)
        } else {
            None
        };
        let target_peak = self.target_peak_dbfs;
        let automixer = self.automixer;

        let tx = self.sender.clone();
        let ffmpeg_path = self.ffmpeg_path.clone();
        let cancel = Arc::clone(&self.cancel_flag);

        thread::spawn(move || {
            let _ = tx.send(AppMsg::Log("Scanning directory...".into()));
            ctx.request_repaint();

            let files: Vec<PathBuf> = WalkDir::new(&input_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && is_audio_file(e.path()))
                .map(|e| e.path().to_path_buf())
                .collect();

            let total = files.len();
            let _ = tx.send(AppMsg::Progress(0, total));
            ctx.request_repaint();

            for (i, file) in files.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    let _ = tx.send(AppMsg::Log("Processing stopped by user.".into()));
                    let _ = tx.send(AppMsg::Stopped);
                    ctx.request_repaint();
                    return;
                }

                match process_single_file(
                    file,
                    &input_path,
                    &output_path,
                    lufs_option,
                    target_peak,
                    automixer,
                    &ffmpeg_path,
                ) {
                    Err(e) => {
                        let _ = tx.send(AppMsg::Error(format!(
                            "Error {}: {}",
                            file.display(),
                            e
                        )));
                    }
                    Ok(norm_result) => {
                        let msg = match norm_result {
                            NormResult::Standard => {
                                format!("Processed (LUFS 2-pass): {}", file.display())
                            }
                            NormResult::Padded => {
                                format!(
                                    "Processed (LUFS 2-pass + padding): {}",
                                    file.display()
                                )
                            }
                            NormResult::Peak { gain_db } => {
                                format!(
                                    "Processed (peak norm, gain {:.1} dB): {}",
                                    gain_db,
                                    file.display()
                                )
                            }
                            NormResult::Skipped => {
                                format!(
                                    "Converted without normalization (silent/empty): {}",
                                    file.display()
                                )
                            }
                        };
                        let _ = tx.send(AppMsg::Log(msg));
                    }
                }
                let _ = tx.send(AppMsg::Progress(i + 1, total));
                ctx.request_repaint();
            }

            let _ = tx.send(AppMsg::Log("Processing finished.".into()));
            let _ = tx.send(AppMsg::Finished);
            ctx.request_repaint();
        });
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.receiver.try_recv() {
            match msg {
                AppMsg::Log(text) => self.logs.push(text),
                AppMsg::Error(text) => self.logs.push(format!("[ERROR] {}", text)),
                AppMsg::Progress(current, total) => {
                    self.current_progress = current;
                    self.total_files = total;
                }
                AppMsg::Finished => {
                    self.is_processing = false;
                    self.is_analyzing = false;
                }
                AppMsg::Stopped => {
                    self.is_processing = false;
                    self.is_analyzing = false;
                }
                AppMsg::AnalysisResult(avg) => {
                    self.average_lufs = Some(avg);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for AudioBatchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Voice-Over Compression and Normalization (ADPCM 4-bit)");

            ui.add_space(10.0);
            ui.group(|ui| {
                ui.label("Path Settings:");
                ui.horizontal(|ui| {
                    ui.label("Source: ");
                    ui.text_edit_singleline(&mut self.input_dir);
                    if ui.button("Browse...").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.input_dir = path.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Output: ");
                    ui.text_edit_singleline(&mut self.output_dir);
                    if ui.button("Browse...").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.output_dir = path.display().to_string();
                        }
                    }
                });
            });

            ui.add_space(10.0);

            ui.group(|ui| {
                ui.label("Loudness:");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.normalize_volume, "Normalize volume");
                    if ui
                        .button("Analyze folder loudness...")
                        .on_hover_text("Select a folder to check its average loudness level")
                        .clicked()
                    {
                        self.start_analysis(ctx.clone());
                    }
                });

                if let Some(avg) = self.average_lufs {
                    ui.colored_label(
                        egui::Color32::KHAKI,
                        format!("Average level of your files: {:.2} LUFS", avg),
                    );
                    if ui.button("Set as target").clicked() {
                        self.target_lufs = avg.round();
                    }
                }

                ui.add_enabled_ui(self.normalize_volume, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.target_lufs, -23.0..=-6.0)
                            .text("Target LUFS-I (files >= 1s)"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.target_peak_dbfs, -12.0..=-1.0)
                            .text("Target peak dBFS (files < 1s)"),
                    )
                    .on_hover_text(
                        "Peak normalization used for very short samples (< 1s).\n\
                         Recommended: -3 dBFS (safe headroom for 4-bit ADPCM).",
                    );
                });
            });

            ui.add_space(10.0);

            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.automixer, "Automixer");
                    ui.label(
                        egui::RichText::new("(EQ → De-esser → Compressor, applied before normalization)")
                            .weak()
                            .italics(),
                    );
                });
                if self.automixer {
                    ui.add_space(4.0);
                    let warning = "⚠  Attention! There is no way to create a universal mixing \
                        tool. This is the closest I can think of to a universal mixing chain \
                        without doing proper mixing, but the results may drastically vary based \
                        on the provided material. Use with caution!";
                    ui.colored_label(egui::Color32::from_rgb(255, 200, 80), warning);
                }
            });

            ui.add_space(15.0);

            let can_start = !self.is_processing
                && !self.is_analyzing
                && !self.input_dir.is_empty()
                && !self.output_dir.is_empty();
            ui.add_enabled_ui(can_start, |ui| {
                if ui.button("START PROCESSING").clicked() {
                    self.start_processing(ctx.clone());
                }
            });

            if self.is_processing || self.is_analyzing {
                ui.add_space(10.0);
                let label = if self.is_analyzing { "Analyzing" } else { "Processing" };
                let progress = if self.total_files > 0 {
                    self.current_progress as f32 / self.total_files as f32
                } else {
                    0.0
                };
                ui.horizontal(|ui| {
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .text(format!(
                                "{}: {}/{}",
                                label, self.current_progress, self.total_files
                            ))
                            .desired_width(ui.available_width() - 80.0),
                    );
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("⏹ Stop").color(egui::Color32::from_rgb(255, 90, 90)),
                        ))
                        .on_hover_text("Stop after the current file finishes")
                        .clicked()
                    {
                        self.cancel_flag.store(true, Ordering::Relaxed);
                        self.logs.push("Stop requested — waiting for current file...".into());
                    }
                });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.label("Logs:");

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for log in &self.logs {
                        let color = if log.starts_with("[ERROR]") {
                            egui::Color32::LIGHT_RED
                        } else {
                            egui::Color32::GRAY
                        };
                        ui.colored_label(color, log);
                    }
                });
        });
    }
}

// ---------------------------------------------------------------------------
// FFmpeg helpers — Detection
// ---------------------------------------------------------------------------

/// Returns `true` for file extensions that ffmpeg can decode as audio.
/// Used to skip sidecar files (.reapeaks, .txt, .png, etc.) that WalkDir picks up.
fn is_audio_file(path: &Path) -> bool {
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
///
/// Priority:
///   1. `ffmpeg` available on PATH (verified by running `ffmpeg -version`)
///   2. `ffmpeg.exe` / `ffmpeg` sitting next to the current executable
fn find_ffmpeg() -> Result<PathBuf> {
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

// ---------------------------------------------------------------------------
// FFmpeg helpers — Measurements
// ---------------------------------------------------------------------------

/// Returns the sample rate of the source file.
///
/// Loudnorm internally resamples to 192 kHz — without an explicit `-ar`, the
/// ADPCM IMA WAV codec might receive an incorrect sampling frequency.
fn get_sample_rate(input: &Path, ffmpeg: &Path) -> Option<u32> {
    let output = Command::new(ffmpeg)
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
fn get_duration(input: &Path, ffmpeg: &Path) -> Option<f32> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(input)
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        if line.contains("Duration:") {
            // Format: "  Duration: HH:MM:SS.ms, start: ..."
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
///
/// Uses standard EBU R128 without padding — files < ~3s will return `None`.
/// `measured_I` is independent of the selected target, so we use -23 LUFS (EBU R128).
fn measure_lufs(input: &Path, ffmpeg: &Path) -> Result<Option<f32>> {
    let filter = "loudnorm=I=-23:TP=-1.5:LRA=1.0:print_format=json";
    let output = Command::new(ffmpeg)
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
///
/// Returns `None` when `measured_I = -inf` (file too short).
/// The target must be identical to Pass 2 — `target_offset` is calculated relative to it.
fn get_file_stats(
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
    let output = Command::new(ffmpeg)
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
///
/// Pads the signal with silence to `pad_to_secs` seconds, ensuring the
/// EBU R128 algorithm has enough blocks for analysis. Silence is below the
/// absolute gating threshold (-70 LUFS), so it does not distort the `measured_I` result.
///
/// Returns `None` if the measurement fails even with padding.
fn get_file_stats_padded(
    input: &Path,
    ffmpeg: &Path,
    target_lufs: f32,
    pad_to_secs: f32,
    prefix: Option<&str>,
) -> Result<Option<LoudnormStats>> {
    // apad adds silence at the end; atrim trims to pad_to_secs to avoid
    // creating unnecessarily large memory buffers.
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
    let output = Command::new(ffmpeg)
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
fn measure_peak_dbfs(input: &Path, ffmpeg: &Path, prefix: Option<&str>) -> Result<f32> {
    let filter = match prefix {
        Some(p) => format!("{},volumedetect", p),
        None => "volumedetect".to_string(),
    };
    let output = Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", &filter, "-f", "null", "-"])
        .stderr(Stdio::piped())
        .output()
        .context("FFmpeg error during peak measurement")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);

    // Line format: "[Parsed_volumedetect_0 @ ...] max_volume: -3.5 dB"
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

// ---------------------------------------------------------------------------
// Automixer chain
// ---------------------------------------------------------------------------

/// Builds a fixed processing filter chain intended as a "closest-to-universal"
/// mixing pre-processor for voice-over material.
///
/// Chain order: EQ → De-esser → Compressor
///
/// EQ bands:
///   - HPF  70 Hz, Q 1.0, 18 dB/oct (3-pole) — remove rumble and proximity buildup
///   - Bell  90 Hz, Q 2.478, −2.0 dB          — tighten low-mid mud
///   - Bell 175 Hz, Q 1.0,   −2.22 dB         — reduce boxy buildup
///   - Bell 360 Hz, Q 1.0,   −1.23 dB         — reduce nasal honk
///   - Bell 1350 Hz, Q 1.4,  +1.4 dB          — add presence / intelligibility
///   - Bell 4246 Hz, Q 2.0,  −1.36 dB         — tame harshness
///   - High shelf 8000 Hz, Q 1.0, +1.0 dB     — gentle air boost
///
/// De-esser: balanced intensity, targeting the 6–8 kHz sibilance band.
/// Compressor: heavy (−12 dB threshold, 4:1 ratio) with 4 dB makeup gain.
fn automixer_filter_chain() -> String {
    // 18 dB/oct HPF at 70 Hz:
    // ffmpeg's `highpass` supports only 1 or 2 poles natively.
    // We cascade a 2-pole (12 dB/oct, Q=1.0) and a 1-pole (6 dB/oct) to get 18 dB/oct
    // while preserving the Q=1.0 character on the resonant section.
    let hpf = "highpass=f=70:poles=2:width_type=q:width=1.0,highpass=f=70:poles=1";

    // Bell / peaking filters via `equalizer` (width_type=q → Q factor).
    let eq_90   = "equalizer=f=90:width_type=q:width=2.478:g=-2.0";
    let eq_175  = "equalizer=f=175:width_type=q:width=1.0:g=-2.22";
    let eq_360  = "equalizer=f=360:width_type=q:width=1.0:g=-1.23";
    let eq_1350 = "equalizer=f=1350:width_type=q:width=1.4:g=1.4";
    let eq_4246 = "equalizer=f=4246:width_type=q:width=2.0:g=-1.36";

    // High shelf at 8 kHz.
    let shelf_8k = "highshelf=f=8000:width_type=q:width=1.0:g=1.0";

    // De-esser — balanced: intensity 0.4, max de-essing 0.5, center ~0.5 (≈ 7 kHz).
    // s=o → output the de-essed signal (not the raw side chain).
    let deesser = "deesser=i=0.4:m=0.5:f=0.5:s=o";

    // Compressor — heavy but musical.
    //   threshold: −12 dBFS → linear amplitude ≈ 0.251  (10^(−12/20))
    //   ratio: 4:1
    //   attack: 5 ms  — fast enough to catch transients without choking them
    //   release: 80 ms — natural decay for speech
    //   makeup: 4 dB  — compensate for gain reduction
    let comp = "acompressor=threshold=0.251:ratio=4:attack=5:release=80:makeup=4";

    // hpf already contains a comma-separated pair, so join everything manually.
    format!(
        "{},{},{},{},{},{},{},{},{}",
        hpf, eq_90, eq_175, eq_360, eq_1350, eq_4246, shelf_8k, deesser, comp
    )
}

// ---------------------------------------------------------------------------
// File Processing
// ---------------------------------------------------------------------------

/// Processes a single file: hybrid normalization + conversion to ADPCM IMA WAV.
///
/// Normalization strategy (when enabled by user):
///
///   1. Attempt standard 2-pass loudnorm (files >= ~3s).
///   2. If measured_I = -inf, check duration:
///      a. >= 1s  -> 2-pass loudnorm with silence padding to max(5s, duration+1s).
///                   Pass 2 operates on the ORIGINAL — padded silence is gated out.
///      b. < 1s   -> Peak normalization to `target_peak_dbfs`.
///                   Gain capped at +40 dB to avoid amplifying noise/silence.
///   3. Extreme fallback (silent/empty signal despite padding): convert without normalization.
fn process_single_file(
    input: &Path,
    input_base: &Path,
    output_base: &Path,
    target_lufs: Option<f32>,
    target_peak_dbfs: f32,
    automixer: bool,
    ffmpeg: &Path,
) -> Result<NormResult> {
    let rel_path = input.strip_prefix(input_base)?;
    let output = output_base.join(rel_path).with_extension("wav");

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let source_sr = get_sample_rate(input, ffmpeg).unwrap_or(44100);

    // Build optional automixer prefix — applied before any normalization filter.
    let automixer_chain: Option<String> = if automixer {
        Some(automixer_filter_chain())
    } else {
        None
    };
    let prefix: Option<&str> = automixer_chain.as_deref();

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"]).arg(input).arg("-vn");

    let norm_result = if let Some(lufs) = target_lufs {
        // --- Attempt 1: standard 2-pass ---
        match get_file_stats(input, ffmpeg, lufs, prefix)? {
            Some(stats) => {
                apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, prefix);
                NormResult::Standard
            }
            None => {
                let duration = get_duration(input, ffmpeg).unwrap_or(0.0);

                if duration >= 1.0 {
                    // --- Attempt 2: 2-pass with padding (files ~1-3s) ---
                    let pad_secs = f32::max(5.0, duration + 1.0);
                    match get_file_stats_padded(input, ffmpeg, lufs, pad_secs, prefix)? {
                        Some(stats) => {
                            apply_loudnorm_pass2(&mut cmd, lufs, &stats, source_sr, prefix);
                            NormResult::Padded
                        }
                        None => {
                            // Even with padding, the signal is inaudible.
                            if let Some(p) = prefix {
                                cmd.args(["-af", p]);
                            }
                            NormResult::Skipped
                        }
                    }
                } else {
                    // --- Attempt 3: peak normalization (files < 1s) ---
                    match measure_peak_dbfs(input, ffmpeg, prefix) {
                        Ok(peak_dbfs) if peak_dbfs.is_finite() => {
                            let gain_db = target_peak_dbfs - peak_dbfs;
                            if gain_db <= 40.0 {
                                let vol_filter = format!("volume={gain:.4}dB", gain = gain_db);
                                let filter = match prefix {
                                    Some(p) => format!("{},{}", p, vol_filter),
                                    None => vol_filter,
                                };
                                cmd.args(["-af", &filter]);
                                NormResult::Peak { gain_db }
                            } else {
                                if let Some(p) = prefix {
                                    cmd.args(["-af", p]);
                                }
                                NormResult::Skipped
                            }
                        }
                        _ => {
                            if let Some(p) = prefix {
                                cmd.args(["-af", p]);
                            }
                            NormResult::Skipped
                        }
                    }
                }
            }
        }
    } else {
        // Normalization disabled by user.
        if let Some(p) = prefix {
            cmd.args(["-af", p]);
        }
        NormResult::Skipped
    };

    cmd.args(["-c:a", "adpcm_ima_wav"]).arg(&output);

    let final_output = cmd
        .stderr(Stdio::piped())
        .output()
        .context("Failed to run FFmpeg (Conversion)")?;

    if !final_output.status.success() {
        let err = String::from_utf8_lossy(&final_output.stderr);
        return Err(anyhow!("FFmpeg Error: {}", err));
    }

    Ok(norm_result)
}

/// Applies Pass 2 loudnorm arguments to an existing Command.
///
/// Extracted to a separate function — used for both standard and padded paths,
/// as the FFmpeg arguments are identical.
///
/// LRA=1.0 — very narrow dynamic range, minimizes 4-bit ADPCM quantization errors.
/// linear=true — clean linear gain without extra compression in Pass 2.
/// -ar — restores original sample rate after internal loudnorm resampling (192 kHz).
fn apply_loudnorm_pass2(
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

fn extract_loudnorm_stats(stderr: &str) -> Result<LoudnormStats> {
    let start_idx = stderr.rfind('{').context("Missing JSON in FFmpeg output")?;
    let end_idx = stderr.rfind('}').context("Missing JSON in FFmpeg output")?;
    let json_str = &stderr[start_idx..=end_idx];
    let stats: LoudnormStats = serde_json::from_str(json_str)?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    let ffmpeg_path = find_ffmpeg().unwrap_or_else(|e| {
        eprintln!("{}", e);
        PathBuf::from("ffmpeg")
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([700.0, 700.0]),
        ..Default::default()
    };

    eframe::run_native(
        "VOCAN",
        options,
        Box::new(move |_cc| Box::new(AudioBatchApp::new(ffmpeg_path))),
    )
}