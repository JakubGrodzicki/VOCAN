use anyhow::{anyhow, Context, Result};
use eframe::egui;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use walkdir::WalkDir;

#[derive(Deserialize, Debug)]
struct LoudnormStats {
    input_i: String,
    input_tp: String,
    input_lra: String,
    input_thresh: String,
    target_offset: String,
}

enum AppMsg {
    Log(String),
    Progress(usize, usize),
    Error(String),
    Finished,
    AnalysisResult(f32),
}

struct AudioBatchApp {
    input_dir: String,
    output_dir: String,
    normalize_volume: bool,
    target_lufs: f32,
    is_processing: bool,
    is_analyzing: bool,
    average_lufs: Option<f32>,
    logs: Vec<String>,
    current_progress: usize,
    total_files: usize,
    receiver: Receiver<AppMsg>,
    sender: Sender<AppMsg>,
    ffmpeg_path: PathBuf,
}

impl AudioBatchApp {
    fn new(ffmpeg_path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            input_dir: String::new(),
            output_dir: String::new(),
            normalize_volume: false,
            target_lufs: -14.0,
            is_processing: false,
            is_analyzing: false,
            average_lufs: None,
            logs: Vec::new(),
            current_progress: 0,
            total_files: 0,
            receiver,
            sender,
            ffmpeg_path,
        }
    }

    fn start_analysis(&mut self, ctx: egui::Context) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            let folder_path = path.to_path_buf();
            self.is_analyzing = true;
            self.average_lufs = None;
            self.logs.push(format!("Started folder analysis: {}", folder_path.display()));

            let tx = self.sender.clone();
            let ffmpeg_path = self.ffmpeg_path.clone();
            thread::spawn(move || {
                let files: Vec<PathBuf> = WalkDir::new(&folder_path)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.path().to_path_buf())
                    .collect();

                let total = files.len();
                let mut sum_lufs = 0.0;
                let mut count = 0;

                for (i, file) in files.iter().enumerate() {
                    let _ = tx.send(AppMsg::Progress(i + 1, total));
                    ctx.request_repaint();

                    if let Ok(stats) = get_file_stats(file, &ffmpeg_path) {
                        if let Ok(val) = stats.input_i.parse::<f32>() {
                            sum_lufs += val;
                            count += 1;
                        }
                    }
                }

                if count > 0 {
                    let avg = sum_lufs / count as f32;
                    let _ = tx.send(AppMsg::AnalysisResult(avg));
                    let _ = tx.send(AppMsg::Log(format!("Analysis finished. Average LUFS: {:.2}", avg)));
                } else {
                    let _ = tx.send(AppMsg::Error("No audio files found for analysis.".into()));
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

        let input_path = PathBuf::from(&self.input_dir);
        let output_path = PathBuf::from(&self.output_dir);

        let lufs_option = if self.normalize_volume {
            Some(self.target_lufs)
        } else {
            None
        };

        let tx = self.sender.clone();
        let ffmpeg_path = self.ffmpeg_path.clone();

        thread::spawn(move || {
            let _ = tx.send(AppMsg::Log("Scanning directory...".into()));
            ctx.request_repaint();

            let files: Vec<PathBuf> = WalkDir::new(&input_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.path().to_path_buf())
                .collect();

            let total = files.len();
            let _ = tx.send(AppMsg::Progress(0, total));
            ctx.request_repaint();

            for (i, file) in files.iter().enumerate() {
                if let Err(e) = process_single_file(file, &input_path, &output_path, lufs_option, &ffmpeg_path) {
                    let _ = tx.send(AppMsg::Error(format!("Error {}: {}", file.display(), e)));
                } else {
                    let _ = tx.send(AppMsg::Log(format!("Processed: {}", file.display())));
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
                AppMsg::AnalysisResult(avg) => {
                    self.average_lufs = Some(avg);
                }
            }
        }
    }
}

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

                    if ui.button("Analyze folder loudness...").on_hover_text("Select a folder to check its average loudness level").clicked() {
                        self.start_analysis(ctx.clone());
                    }
                });

                if let Some(avg) = self.average_lufs {
                    ui.colored_label(egui::Color32::KHAKI, format!("Average level of your files: {:.2} LUFS", avg));
                    if ui.button("Set as target").clicked() {
                        self.target_lufs = avg.round();
                    }
                }

                ui.add_enabled_ui(self.normalize_volume, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.target_lufs, -23.0..=-6.0)
                            .text("Target LUFS-I"),
                    );
                });
            });

            ui.add_space(15.0);

            let can_start = !self.is_processing && !self.is_analyzing && !self.input_dir.is_empty() && !self.output_dir.is_empty();
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
                ui.add(egui::ProgressBar::new(progress).text(format!(
                    "{}: {}/{}",
                    label, self.current_progress, self.total_files
                )));
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

/// Resolves the ffmpeg executable path.
///
/// Priority:
///   1. `ffmpeg` available on PATH (verified by running `ffmpeg -version`)
///   2. `ffmpeg.exe` / `ffmpeg` sitting next to the current executable
///
/// Returns an error if neither location yields a working binary.
fn find_ffmpeg() -> Result<PathBuf> {
    // 1. Try PATH first.
    let path_probe = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if path_probe.map(|s| s.success()).unwrap_or(false) {
        return Ok(PathBuf::from("ffmpeg"));
    }

    // 2. Try the directory that contains our own .exe.
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // On Windows the binary is named ffmpeg.exe; on Unix just ffmpeg.
            let candidate = exe_dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });

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

fn get_file_stats(input: &Path, ffmpeg: &Path) -> Result<LoudnormStats> {
    let filter = "loudnorm=I=-14:TP=-1.5:LRA=1.0:print_format=json";
    let output = Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-i"])
        .arg(input)
        .args(["-vn", "-af", filter, "-f", "null", "-"])
        .stderr(Stdio::piped())
        .output()
        .context("FFmpeg error during analysis")?;

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    extract_loudnorm_stats(&stderr_str)
}

fn process_single_file(
    input: &Path,
    input_base: &Path,
    output_base: &Path,
    target_lufs: Option<f32>,
    ffmpeg: &Path,
) -> Result<()> {
    let rel_path = input.strip_prefix(input_base)?;
    let output = output_base.join(rel_path).with_extension("wav");

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-i"]).arg(input).arg("-vn");

    if let Some(lufs) = target_lufs {
        let stats = get_file_stats(input, ffmpeg)?;
        let filter_pass2 = format!(
            "loudnorm=I={}:TP=-1.5:LRA=1.0:measured_I={}:measured_LRA={}:measured_TP={}:measured_thresh={}:offset={}",
            lufs, stats.input_i, stats.input_lra, stats.input_tp, stats.input_thresh, stats.target_offset
        );
        cmd.args(["-af", &filter_pass2]);
    }

    cmd.args(["-c:a", "adpcm_ima_wav"]).arg(&output);

    let final_output = cmd
        .stderr(Stdio::piped())
        .output()
        .context("Failed to run FFmpeg (Conversion)")?;

    if !final_output.status.success() {
        let err = String::from_utf8_lossy(&final_output.stderr);
        return Err(anyhow!("FFmpeg Error: {}", err));
    }

    Ok(())
}

fn extract_loudnorm_stats(stderr: &str) -> Result<LoudnormStats> {
    let start_idx = stderr.rfind('{').context("Missing JSON in FFmpeg output")?;
    let end_idx = stderr.rfind('}').context("Missing JSON in FFmpeg output")?;
    let json_str = &stderr[start_idx..=end_idx];
    let stats: LoudnormStats = serde_json::from_str(json_str)?;
    Ok(stats)
}

fn main() -> eframe::Result<()> {
    let ffmpeg_path = find_ffmpeg().unwrap_or_else(|e| {
        eprintln!("{}", e);
        // Surface the error inside the GUI instead of hard-crashing.
        // The app will start but all operations will fail with a clear message.
        PathBuf::from("ffmpeg")
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([700.0, 650.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Audio Batch Converter Pro",
        options,
        Box::new(move |_cc| Box::new(AudioBatchApp::new(ffmpeg_path))),
    )
}