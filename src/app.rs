use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc::{self, Receiver, Sender}};
use std::thread;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::ffmpeg::{is_audio_file, measure_lufs};
use crate::processing::process_single_file;
use crate::types::{AppMsg, NormResult};

// ---------------------------------------------------------------------------
// Application State
// ---------------------------------------------------------------------------

pub struct AudioBatchApp {
    input_dir: String,
    output_dir: String,
    normalize_volume: bool,
    target_lufs: f32,
    /// Target peak (dBFS) used for files < 1s (peak normalization).
    target_peak_dbfs: f32,
    /// Applies a fixed processing chain (EQ → De-esser → Compressor) before normalization.
    automixer: bool,
    /// Module 1: custom spectral gate (FFT).
    automixer_spectral_gate: bool,
    /// Module 3: nnnoiseless used as pseudo-dereverb.
    automixer_nn_dereverb: bool,
    /// Module 4: DeepFilterNet3 — proper dereverb (+ optional denoise).
    automixer_dfn3_dereverb: bool,
    automixer_dfn3_mix: f32,
    automixer_dfn3_postfilter: bool,
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
    pub fn new(ffmpeg_path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            input_dir: String::new(),
            output_dir: String::new(),
            normalize_volume: false,
            target_lufs: -14.0,
            target_peak_dbfs: -3.0,
            automixer: false,
            automixer_spectral_gate: false,
            automixer_nn_dereverb: false,
            automixer_dfn3_dereverb: false,
            automixer_dfn3_mix: 0.8,
            automixer_dfn3_postfilter: false,
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
                let done = Arc::new(AtomicUsize::new(0));
                let sum = Arc::new(std::sync::Mutex::new((0.0f32, 0usize))); // (sum, count)

                files.par_iter().for_each(|file| {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let tx = tx.clone();

                    match measure_lufs(file, &ffmpeg_path) {
                        Ok(Some(val)) => {
                            let mut guard = sum.lock().unwrap();
                            guard.0 += val;
                            guard.1 += 1;
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

                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(AppMsg::Progress(n, total));
                    ctx.request_repaint();
                });

                if cancel.load(Ordering::Relaxed) {
                    let _ = tx.send(AppMsg::Log("Analysis stopped by user.".into()));
                    let _ = tx.send(AppMsg::Stopped);
                    ctx.request_repaint();
                    return;
                }

                let (s, c) = *sum.lock().unwrap();
                if c > 0 {
                    let avg = s / c as f32;
                    let _ = tx.send(AppMsg::AnalysisResult(avg));
                    let _ = tx.send(AppMsg::Log(format!(
                        "Analysis finished. Average LUFS: {:.2} (from {} files)",
                        avg, c
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
        let automixer_sg = self.automixer_spectral_gate;
        let automixer_nn = self.automixer_nn_dereverb;
        let automixer_dfn3 = self.automixer_dfn3_dereverb;
        let automixer_dfn3_mix = self.automixer_dfn3_mix;
        let automixer_dfn3_pf = self.automixer_dfn3_postfilter;

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

            let done = Arc::new(AtomicUsize::new(0));

            files.par_iter().for_each(|file| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let tx = tx.clone();

                match process_single_file(
                    file,
                    &input_path,
                    &output_path,
                    lufs_option,
                    target_peak,
                    automixer,
                    automixer_sg,
                    automixer_nn,
                    automixer_dfn3,
                    automixer_dfn3_mix,
                    automixer_dfn3_pf,
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
                                format!("Processed (LUFS 2-pass + padding): {}", file.display())
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

                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = tx.send(AppMsg::Progress(n, total));
                ctx.request_repaint();
            });

            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(AppMsg::Log("Processing stopped by user.".into()));
                let _ = tx.send(AppMsg::Stopped);
            } else {
                let _ = tx.send(AppMsg::Log("Processing finished.".into()));
                let _ = tx.send(AppMsg::Finished);
            }
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
                        egui::RichText::new("(De-esser -> EQ -> Compressor, applied before normalization)")
                            .weak()
                            .italics(),
                    );
                });

                // Additional options (active only when automixer is enabled)
                ui.add_enabled_ui(self.automixer && !self.is_processing, |ui| {
                    ui.indent("automixer_opts", |ui| {
                        // SG and NN are mutually exclusive (both live in the "noise" slot).
                        // DFN3 is independent — it's proper dereverb, runs before the others.
                        let sg_disabled = self.automixer_nn_dereverb;
                        let nn_disabled = self.automixer_spectral_gate;

                        ui.add_enabled_ui(!sg_disabled, |ui| {
                            if ui
                                .checkbox(
                                    &mut self.automixer_spectral_gate,
                                    "Intelligent noise removal (Spectral Gate)",
                                )
                                .changed()
                                && self.automixer_spectral_gate
                            {
                                self.automixer_nn_dereverb = false;
                            }
                        });

                        ui.add_enabled_ui(!nn_disabled, |ui| {
                            if ui
                                .checkbox(
                                    &mut self.automixer_nn_dereverb,
                                    "Noise reduction (nnnoiseless)",
                                )
                                .changed()
                                && self.automixer_nn_dereverb
                            {
                                self.automixer_spectral_gate = false;
                            }
                        });

                        ui.separator();
                        ui.checkbox(
                            &mut self.automixer_dfn3_dereverb,
                            "Dereverb (DeepFilterNet3)",
                        );
                        ui.add_enabled_ui(self.automixer_dfn3_dereverb, |ui| {
                            ui.add(
                                egui::Slider::new(&mut self.automixer_dfn3_mix, 0.0..=1.0)
                                    .text("Dereverb mix")
                                    .fixed_decimals(2),
                            );
                            ui.checkbox(
                                &mut self.automixer_dfn3_postfilter,
                                "Post-filter (aggressive)",
                            );
                        });

                        ui.label(
                            egui::RichText::new("Voice EQ works automatically (50% strength)")
                                .small()
                                .italics(),
                        );
                    });
                });

                if self.automixer {
                    ui.add_space(4.0);
                    let warning = "\u{26a0}  Attention! There is no way to create a universal mixing \
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
                            egui::RichText::new("\u{23f9} Stop").color(egui::Color32::from_rgb(255, 90, 90)),
                        ))
                        .on_hover_text("Stop after the current file finishes")
                        .clicked()
                    {
                        self.cancel_flag.store(true, Ordering::Relaxed);
                        self.logs.push("Stop requested \u{2014} waiting for current file...".into());
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
