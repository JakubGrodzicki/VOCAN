use eframe::egui;
use rayon::prelude::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread;
use walkdir::WalkDir;

use crate::ffmpeg::{is_audio_file, measure_lufs};
use crate::processing::{output_path_for, process_single_file};
use crate::types::{AppMsg, NormResult, OutputFormat, ProcessingOptions, ReductionProfile};

// ---------------------------------------------------------------------------
// Application State
// ---------------------------------------------------------------------------

pub struct AudioBatchApp {
    input_dir: String,
    output_dir: String,
    normalize_volume: bool,
    target_lufs: f32,
    /// Target peak (dBFS) used as a peak-normalization fallback when EBU R128
    /// loudness measurement (standard or padded) fails.
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
    /// Module 5: smart downward expander (noise-floor-based, bounded).
    automixer_expander: bool,
    /// 0–100, UI-facing "Safety Margin". Higher = more conservative.
    automixer_expander_safety_pct: f32,
    /// Preset reduction depth (Safe/Recommended/Hard/Max).
    automixer_expander_reduction_profile: ReductionProfile,
    /// Output format selector (ADPCM, PCM, FLAC, MP3, OGG).
    output_format: OutputFormat,
    /// Bitrate for lossy formats (MP3, OGG).
    bitrate_kbps: u32,
    is_processing: bool,
    is_analyzing: bool,
    average_lufs: Option<f32>,
    logs: VecDeque<String>,
    current_progress: usize,
    total_files: usize,
    receiver: Receiver<AppMsg>,
    sender: Sender<AppMsg>,
    ffmpeg_path: PathBuf,
    /// Why `find_ffmpeg` failed, if it did.
    ///
    /// The binary is built with `windows_subsystem = "windows"`, so there is no
    /// console for the startup `eprintln!` this replaces: the message went
    /// nowhere and the user was left with an app whose every file failed with an
    /// opaque "FFmpeg spawn failed".
    ffmpeg_error: Option<String>,
    /// Shared cancellation flag — set to `true` to request the worker thread to stop.
    cancel_flag: Arc<AtomicBool>,
}

/// Maximum number of log lines retained.
///
/// A batch produces at least one line per file, so an unbounded log grows
/// without limit -- and the log pane re-lays-out every retained line on every
/// frame, which makes the UI crawl exactly when the user wants to scroll back
/// through a long run's errors.
const MAX_LOG_LINES: usize = 5_000;

/// Maximum length of a single log line, in characters.
///
/// A failing FFmpeg invocation appends its whole stderr, which can run to
/// several kilobytes; the useful part is at the front.
const MAX_LOG_LINE_CHARS: usize = 2_000;

/// Returns `true` if `a` and `b` refer to the same directory on disk.
///
/// Compares canonicalized paths (resolving `.`/`..`/symlinks) where possible,
/// falling back to plain path equality if either side can't be canonicalized
/// (e.g. it doesn't exist yet). Used to stop the user from pointing the
/// output folder at the source folder: with the default ADPCM output format
/// (which keeps the `.wav` extension), that makes every output path
/// byte-identical to its input path, and FFmpeg reading and writing the same
/// file at once risks corrupting the original source audio.
fn same_folder(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        // At least one path could not be resolved (typically: it does not exist
        // yet). Raw `==` is case-sensitive even on Windows, where the
        // filesystem is not -- so `C:\Out` and `C:\out`, the very same folder,
        // used to compare unequal and walk straight past this guard.
        _ => paths_equal_ignoring_platform_case(a, b),
    }
}

#[cfg(windows)]
fn paths_equal_ignoring_platform_case(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

#[cfg(not(windows))]
fn paths_equal_ignoring_platform_case(a: &Path, b: &Path) -> bool {
    a == b
}

/// Returns `true` if `output` sits *inside* `input`'s tree.
///
/// The single run in front of us would survive this (the file list is collected
/// before any output is written), but the *next* one would not: the scan would
/// pick up this run's results as fresh input and re-process them into
/// `out/out/...`, deeper each time, quietly polluting the source tree.
fn output_nested_in_input(input: &Path, output: &Path) -> bool {
    match (input.canonicalize(), output.canonicalize()) {
        (Ok(ci), Ok(co)) => co != ci && co.starts_with(&ci),
        _ => false,
    }
}

/// Validates the source/output folder pair, returning the message to log when
/// the pair is unusable.
///
/// Creates the output folder as a side effect, deliberately: `canonicalize`
/// cannot resolve a path that does not exist, and without it both checks above
/// fall back to weaker textual comparisons.
fn validate_folders(input: &Path, output: &Path) -> Result<(), String> {
    if !input.is_dir() {
        return Err(format!(
            "Source folder does not exist, or is not a folder: {}",
            input.display()
        ));
    }
    if let Err(e) = std::fs::create_dir_all(output) {
        return Err(format!(
            "Cannot create the output folder {}: {}",
            output.display(),
            e
        ));
    }
    if same_folder(input, output) {
        return Err(
            "Source and output folders must be different -- processing in place \
             risks overwriting your original files."
                .to_string(),
        );
    }
    if output_nested_in_input(input, output) {
        return Err(format!(
            "The output folder is inside the source folder ({} is under {}). \
             This run would work, but the next one would pick up these results \
             as new source files and nest them deeper each time.",
            output.display(),
            input.display()
        ));
    }
    Ok(())
}

/// Guarantees that a worker thread reports a terminal [`AppMsg`], even if it
/// unwinds.
///
/// `AppMsg::Finished`/`Stopped` is what clears `is_processing`/`is_analyzing`
/// in [`AudioBatchApp::handle_messages`]. The per-file `catch_unwind` inside
/// the rayon closure does not cover the whole thread: rayon re-raises a worker
/// panic in whichever thread called `par_iter().for_each`, and `sum.lock()` /
/// `tx.clone()` sit outside it entirely. Without this guard such a panic kills
/// the worker thread with no terminal message ever sent, and the UI stays stuck
/// with `is_processing == true` forever -- settings disabled, START greyed out,
/// and Stop only setting the cancel flag without clearing the flag itself. The
/// only way out was killing the application.
///
/// Unwinding runs destructors (the release profile deliberately does not set
/// `panic = "abort"`), so `Drop` still fires on the panic path.
struct CompletionGuard {
    tx: Sender<AppMsg>,
    ctx: egui::Context,
    armed: bool,
}

impl CompletionGuard {
    fn new(tx: Sender<AppMsg>, ctx: egui::Context) -> Self {
        Self {
            tx,
            ctx,
            armed: true,
        }
    }

    /// Call once the thread has sent its own terminal message on the normal path.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.tx.send(AppMsg::Error(
            "Worker thread ended unexpectedly; the run was aborted. \
             Any remaining files were not processed."
                .into(),
        ));
        let _ = self.tx.send(AppMsg::Finished);
        self.ctx.request_repaint();
    }
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
            automixer_dfn3_mix: 80.0,
            automixer_dfn3_postfilter: false,
            automixer_expander: false,
            automixer_expander_safety_pct: 50.0,
            automixer_expander_reduction_profile: ReductionProfile::Recommended,
            output_format: OutputFormat::default(),
            bitrate_kbps: 128,
            is_processing: false,
            is_analyzing: false,
            average_lufs: None,
            logs: VecDeque::new(),
            current_progress: 0,
            total_files: 0,
            receiver,
            sender,
            ffmpeg_path,
            ffmpeg_error: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Builds the app from the result of [`crate::ffmpeg::find_ffmpeg`],
    /// keeping the failure reason so the UI can show it.
    ///
    /// Falls back to the bare name `ffmpeg` so that a user who installs it
    /// while the app is open can simply retry, rather than having to restart.
    pub fn from_ffmpeg_lookup(found: anyhow::Result<PathBuf>) -> Self {
        match found {
            Ok(path) => Self::new(path),
            Err(e) => {
                let mut app = Self::new(PathBuf::from("ffmpeg"));
                let message = e.to_string();
                app.push_log(format!("[ERROR] {message}"));
                app.ffmpeg_error = Some(message);
                app
            }
        }
    }

    /// Appends one line to the log, truncating over-long lines and evicting
    /// the oldest once [`MAX_LOG_LINES`] is reached.
    fn push_log(&mut self, text: String) {
        let text = match text.char_indices().nth(MAX_LOG_LINE_CHARS) {
            Some((cut, _)) => format!("{}... [truncated]", &text[..cut]),
            None => text,
        };
        if self.logs.len() >= MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(text);
    }

    fn start_analysis(&mut self, ctx: egui::Context) {
        // Guard against re-entrant/concurrent runs: analysis and processing
        // share cancel_flag/logs/progress state, and a Finished/Stopped
        // message from either one clears both is_processing and
        // is_analyzing. The UI already disables this action's button while
        // either flag is set; this check makes the invariant hold
        // regardless of caller, and is cheap enough to always run.
        if self.is_analyzing || self.is_processing {
            return;
        }
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            let folder_path = path.to_path_buf();
            self.is_analyzing = true;
            self.average_lufs = None;
            // Reset the progress counters like `start_processing` does. The log
            // is deliberately *not* cleared here: analysis is a side query run
            // against a folder of the user's choosing, not a fresh batch, so
            // wiping the record of the previous run would lose real information.
            self.current_progress = 0;
            self.total_files = 0;
            self.cancel_flag.store(false, Ordering::Relaxed);
            crate::proc::resume();
            self.push_log(format!(
                "Started folder analysis: {}",
                folder_path.display()
            ));

            let tx = self.sender.clone();
            let ffmpeg_path = self.ffmpeg_path.clone();
            let cancel = Arc::clone(&self.cancel_flag);
            thread::spawn(move || {
                let mut completion = CompletionGuard::new(tx.clone(), ctx.clone());
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

                    // Catch panics so a single failure doesn't poison the Mutex
                    // or leave the UI stuck.
                    let result = std::panic::catch_unwind(|| measure_lufs(file, &ffmpeg_path));

                    match result {
                        Ok(Ok(Some(val))) => {
                            // Use unwrap_or_else to handle potential mutex poisoning
                            // from a panicked thread gracefully.
                            let mut guard = sum.lock().unwrap_or_else(|e| e.into_inner());
                            guard.0 += val;
                            guard.1 += 1;
                        }
                        Ok(Ok(None)) => {
                            let _ = tx.send(AppMsg::Log(format!(
                                "Skipped in analysis (too short/quiet): {}",
                                file.display()
                            )));
                        }
                        Ok(Err(e)) => {
                            let _ = tx.send(AppMsg::Log(format!(
                                "Analysis error for {}: {}",
                                file.display(),
                                e
                            )));
                        }
                        Err(_) => {
                            let _ = tx.send(AppMsg::Error(format!(
                                "Panic while analyzing: {}",
                                file.display()
                            )));
                        }
                    }

                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(AppMsg::Progress(n, total));
                    ctx.request_repaint();
                });

                if cancel.load(Ordering::Relaxed) {
                    let _ = tx.send(AppMsg::Log("Analysis stopped by user.".into()));
                    completion.disarm();
                    let _ = tx.send(AppMsg::Stopped);
                    ctx.request_repaint();
                    return;
                }

                let (s, c) = {
                    let guard = sum.lock().unwrap_or_else(|e| e.into_inner());
                    *guard
                };
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
                completion.disarm();
                let _ = tx.send(AppMsg::Finished);
                ctx.request_repaint();
            });
        }
    }

    fn start_processing(&mut self, ctx: egui::Context) {
        // Same re-entrancy guard as `start_analysis`: the two runs share
        // cancel_flag/logs/progress state, and a Finished/Stopped from either
        // clears both flags. The UI's `can_start` already covers this, but the
        // invariant should not depend on the caller.
        if self.is_processing || self.is_analyzing {
            return;
        }

        let input_path = PathBuf::from(&self.input_dir);
        let output_path = PathBuf::from(&self.output_dir);

        if let Err(msg) = validate_folders(&input_path, &output_path) {
            self.push_log(format!("[ERROR] {msg}"));
            return;
        }

        self.is_processing = true;
        self.logs.clear();
        self.current_progress = 0;
        self.total_files = 0;
        self.cancel_flag.store(false, Ordering::Relaxed);
        // Clear the latch a previous Stop left behind, or every child this run
        // spawns would be killed the moment it registers.
        crate::proc::resume();

        let opts = self.processing_options();

        let tx = self.sender.clone();
        let ffmpeg_path = self.ffmpeg_path.clone();
        let cancel = Arc::clone(&self.cancel_flag);

        thread::spawn(move || {
            let mut completion = CompletionGuard::new(tx.clone(), ctx.clone());
            let _ = tx.send(AppMsg::Log("Scanning directory...".into()));
            ctx.request_repaint();

            let scanned: Vec<PathBuf> = WalkDir::new(&input_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && is_audio_file(e.path()))
                .map(|e| e.path().to_path_buf())
                .collect();

            // Two sources in one folder can map to one output: every format
            // rewrites the extension, so `line1.wav` and `line1.mp3` both become
            // `line1.wav`. Since files are processed in parallel, that means two
            // ffmpeg processes writing the same path at the same time -- a
            // corrupted, non-deterministic result with no error anywhere. Drop
            // the duplicates here, loudly, instead.
            let files = {
                let mut claimed: std::collections::HashMap<String, PathBuf> =
                    std::collections::HashMap::new();
                let mut keep = Vec::with_capacity(scanned.len());
                for file in scanned {
                    let out =
                        match output_path_for(&file, &input_path, &output_path, opts.output_format)
                        {
                            Ok(out) => out,
                            Err(e) => {
                                let _ = tx.send(AppMsg::Error(format!(
                                    "Skipping {}: cannot determine its output path: {}",
                                    file.display(),
                                    e
                                )));
                                continue;
                            }
                        };
                    // Windows filenames are case-insensitive, so `A.wav` and
                    // `a.wav` collide there too.
                    let key = if cfg!(windows) {
                        out.to_string_lossy().to_lowercase()
                    } else {
                        out.to_string_lossy().into_owned()
                    };
                    match claimed.get(&key) {
                        Some(first) => {
                            let _ = tx.send(AppMsg::Error(format!(
                                "Skipping {}: it would be written to the same file as {} ({}). \
                                 Rename one of them so both can be converted.",
                                file.display(),
                                first.display(),
                                out.display()
                            )));
                        }
                        None => {
                            claimed.insert(key, file.clone());
                            keep.push(file);
                        }
                    }
                }
                keep
            };

            let total = files.len();
            let _ = tx.send(AppMsg::Progress(0, total));
            ctx.request_repaint();

            let done = Arc::new(AtomicUsize::new(0));

            files.par_iter().for_each(|file| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let tx = tx.clone();

                // Catch panics so a single file failure doesn't kill the worker
                // thread and leave the UI stuck in "processing" forever.
                let result = std::panic::catch_unwind(|| {
                    process_single_file(file, &input_path, &output_path, &opts, &ffmpeg_path)
                });

                match result {
                    Ok(Err(e)) => {
                        let _ = tx.send(AppMsg::Error(format!("Error {}: {}", file.display(), e)));
                    }
                    Ok(Ok(norm_result)) => {
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
                    Err(_) => {
                        let _ = tx.send(AppMsg::Error(format!(
                            "Panic while processing: {}",
                            file.display()
                        )));
                    }
                }

                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = tx.send(AppMsg::Progress(n, total));
                ctx.request_repaint();
            });

            completion.disarm();
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

    /// Snapshots the UI settings into the value the worker threads consume.
    ///
    /// Kept as its own method so `ProcessingOptions::default()` can be checked
    /// against it (see `processing_options_default_matches_the_ui_defaults`) --
    /// the two are separate declarations of the same set of defaults, and
    /// nothing else would notice them drifting apart.
    fn processing_options(&self) -> ProcessingOptions {
        ProcessingOptions {
            target_lufs: if self.normalize_volume {
                Some(self.target_lufs)
            } else {
                None
            },
            target_peak_dbfs: self.target_peak_dbfs,
            automixer: self.automixer,
            automixer_spectral_gate: self.automixer_spectral_gate,
            automixer_nn_dereverb: self.automixer_nn_dereverb,
            automixer_dfn3_dereverb: self.automixer_dfn3_dereverb,
            automixer_dfn3_mix: self.automixer_dfn3_mix,
            automixer_dfn3_postfilter: self.automixer_dfn3_postfilter,
            automixer_expander: self.automixer_expander,
            automixer_expander_safety_pct: self.automixer_expander_safety_pct,
            automixer_expander_reduction_profile: self.automixer_expander_reduction_profile,
            output_format: self.output_format,
            bitrate_kbps: self.bitrate_kbps,
            log: Some(self.sender.clone()),
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.receiver.try_recv() {
            match msg {
                AppMsg::Log(text) => self.push_log(text),
                AppMsg::Error(text) => self.push_log(format!("[ERROR] {}", text)),
                AppMsg::Progress(current, total) => {
                    // Workers bump a shared counter and send immediately after,
                    // so two threads can deliver out of order and make the
                    // displayed count jump backwards. Both runs reset the
                    // counter before starting, so `max` cannot carry a stale
                    // value in from a previous one.
                    self.current_progress = self.current_progress.max(current);
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

        // Closing the window while a batch runs used to leave every ffmpeg (and
        // deep-filter) child of ours running: the process exits without
        // unwinding, so no destructor gets a chance, and Windows gives us no job
        // object to tear the tree down. Those orphans kept writing to output
        // files, which could leave a truncated file that still looks valid.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.cancel_flag.store(true, Ordering::Relaxed);
            crate::proc::terminate_all();
        }

        // Pinned to the bottom of the window so START PROCESSING and the log
        // pane are always visible, regardless of how tall the settings
        // section above (in the CentralPanel) grows. Must be registered
        // before CentralPanel, since CentralPanel always claims whatever
        // space is left after Top/Bottom/Side panels for the frame.
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.add_space(10.0);

            if let Some(err) = &self.ffmpeg_error {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("\u{26a0} {err}\nProcessing will fail until this is fixed."),
                );
                ui.add_space(6.0);
            }

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
                let label = if self.is_analyzing {
                    "Analyzing"
                } else {
                    "Processing"
                };
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
                            egui::RichText::new("\u{23f9} Stop")
                                .color(egui::Color32::from_rgb(255, 90, 90)),
                        ))
                        .on_hover_text(
                            "Stops now: the files still in flight are cancelled \
                             mid-conversion and their partial output is discarded",
                        )
                        .clicked()
                    {
                        self.cancel_flag.store(true, Ordering::Relaxed);
                        // Terminating the running children is what makes Stop
                        // take effect *now*: the worker threads are parked in
                        // `wait()` and cannot see the flag until their
                        // subprocess returns, which for a long file with
                        // DeepFilterNet3 is minutes away.
                        crate::proc::terminate_all();
                        self.push_log("Stop requested \u{2014} finishing up...".into());
                    }
                });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.label("Logs:");

            // Capped height: an unbounded ScrollArea inside a self-sizing
            // TopBottomPanel is circular (the panel wants to fit the
            // ScrollArea, the ScrollArea wants to fill the panel) and
            // reproduces the original clipping bug one level down.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .max_height(180.0)
                .id_source("log_scroll")
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

            ui.add_space(5.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Settings are read-only (visible but not editable) while a
            // batch job is running, instead of only the Automixer options.
            ui.add_enabled_ui(!self.is_processing, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .id_source("settings_scroll")
                    .show(ui, |ui| {
                        ui.heading("Voice-Over Compression and Normalization (ADPCM 4-bit)");

                        ui.add_space(10.0);
                        ui.group(|ui| self.ui_paths(ui));
                        ui.add_space(10.0);
                        ui.group(|ui| self.ui_loudness(ui, ctx));
                        ui.add_space(10.0);
                        ui.group(|ui| self.ui_output_format(ui));
                        ui.add_space(10.0);
                        ui.group(|ui| self.ui_automixer(ui));
                    });
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Settings sections
// ---------------------------------------------------------------------------
//
// Split out of `update` rather than left inline: as one function this was ~340
// lines nested seven closures deep, where the enclosing `if` a given widget
// belongs to is several screens above it.

impl AudioBatchApp {
    fn ui_paths(&mut self, ui: &mut egui::Ui) {
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
    }

    fn ui_loudness(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        {
            ui.label("Loudness:");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.normalize_volume, "Normalize volume");
                // The enclosing settings ScrollArea is already disabled
                // while `is_processing`; additionally guard against
                // `is_analyzing` here so a second click can't start a
                // re-entrant analysis run while one is already in
                // flight (both share cancel_flag/logs/progress state,
                // and AppMsg::Finished/Stopped clear both is_processing
                // and is_analyzing together).
                ui.add_enabled_ui(!self.is_analyzing, |ui| {
                    if ui
                        .button("Analyze folder loudness...")
                        .on_hover_text("Select a folder to check its average loudness level")
                        .clicked()
                    {
                        self.start_analysis(ctx.clone());
                    }
                });
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
                                        .text("Target LUFS-I (EBU R128, padded below 3s)"),
                                );
                                ui.add(
                                    egui::Slider::new(&mut self.target_peak_dbfs, -12.0..=-1.0)
                                        .text("Target peak dBFS (fallback)"),
                                )
                                .on_hover_text(
                                    "Peak normalization fallback, used only when EBU R128 loudness \
                                     measurement (standard or padded) fails -- typically for silent \
                                     or near-silent samples.\n\
                                     Recommended: -3 dBFS (safe headroom for 4-bit ADPCM).",
                                );
                            });
        }
    }

    fn ui_output_format(&mut self, ui: &mut egui::Ui) {
        {
            ui.label("Output Format:");
            egui::ComboBox::from_label("Format")
                .selected_text(self.output_format.label())
                .show_ui(ui, |ui| {
                    for &fmt in OutputFormat::all() {
                        ui.selectable_value(&mut self.output_format, fmt, fmt.label());
                    }
                });
            if self.output_format.needs_bitrate() {
                ui.horizontal(|ui| {
                    ui.label("Bitrate:");
                    egui::ComboBox::from_id_source("bitrate_combo")
                        .selected_text(format!("{} kbps", self.bitrate_kbps))
                        .show_ui(ui, |ui| {
                            for &b in &[36, 48, 64, 128, 256, 320] {
                                ui.selectable_value(
                                    &mut self.bitrate_kbps,
                                    b,
                                    format!("{} kbps", b),
                                );
                            }
                        });
                });
                if self.bitrate_kbps <= 48 {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 80),
                        "Highest compression, requires a verification for quality.",
                    );
                }
            }
            if self.output_format == OutputFormat::AdpcmWav {
                ui.label(
                    egui::RichText::new("Suggested for video game voice-over")
                        .small()
                        .italics(),
                );
            }
        }
    }

    fn ui_automixer(&mut self, ui: &mut egui::Ui) {
        {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.automixer, "Automixer");
                ui.label(
                    egui::RichText::new(
                        "(De-esser -> EQ -> Compressor, applied before normalization)",
                    )
                    .weak()
                    .italics(),
                );
            });

            // Sub-options fully collapse when automixer is off (not just
            // grayed out), so the settings area stays compact by default.
            if self.automixer {
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
                                    if self.automixer_dfn3_dereverb {
                                        ui.add(
                                            egui::Slider::new(&mut self.automixer_dfn3_mix, 0.0..=100.0)
                                                .text("Dereverb mix")
                                                .suffix("%")
                                                .fixed_decimals(0),
                                        );
                                        ui.checkbox(
                                            &mut self.automixer_dfn3_postfilter,
                                            "Post-filter (aggressive)",
                                        );
                                    }

                                    ui.separator();
                                    // Module 5: Downward Expander
                                    ui.horizontal(|ui| {
                                        ui.checkbox(
                                            &mut self.automixer_expander,
                                            "Downward Expander (noise floor)",
                                        );
                                        ui.label(
                                            egui::RichText::new("Compute heavy!")
                                                .small()
                                                .weak()
                                                .italics(),
                                        );
                                    });
                                    if self.automixer_expander {
                                        ui.add(
                                            egui::Slider::new(
                                                &mut self.automixer_expander_safety_pct,
                                                0.0..=100.0,
                                            )
                                            .text("Safety Margin")
                                            .suffix("%")
                                            .fixed_decimals(0),
                                        )
                                        .on_hover_text(
                                            "Higher = safer, touches less material.\n\
                                             50% is a good starting point.\n\
                                             The threshold sits below the detected noise floor\n\
                                             by a margin derived from this setting.",
                                        );

                                        egui::ComboBox::from_label("Reduction profile")
                                            .selected_text(
                                                self.automixer_expander_reduction_profile.label(),
                                            )
                                            .show_ui(ui, |ui| {
                                                for &profile in ReductionProfile::all() {
                                                    ui.selectable_value(
                                                        &mut self.automixer_expander_reduction_profile,
                                                        profile,
                                                        profile.label(),
                                                    );
                                                }
                                            });

                                        if self.automixer_expander_reduction_profile
                                            == ReductionProfile::Max
                                        {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(255, 200, 80),
                                                "\u{26a0} MAX (-32 dB) is aggressive \u{2014} can sound \
                                                 like a hard gate on RMS-detected material below an \
                                                 already-conservative threshold.",
                                            );
                                        }
                                    }

                                    ui.label(
                                        egui::RichText::new(
                                            "Voice EQ works automatically (50% strength)",
                                        )
                                        .small()
                                        .italics(),
                                    );
                                    // The Rust DSP stages all run on a single
                                    // mono channel, so enabling Automixer
                                    // downmixes stereo sources. Documented in
                                    // the README, but the UI is where someone
                                    // about to run a batch will look.
                                    ui.label(
                                        egui::RichText::new(
                                            "Stereo sources are downmixed to mono",
                                        )
                                        .small()
                                        .italics(),
                                    );
                                });

                ui.add_space(4.0);
                let warning =
                                    "\u{26a0}  Attention! There is no way to create a universal mixing \
                                    tool. This is the closest I can think of to a universal mixing chain \
                                    without doing proper mixing, but the results may drastically vary based \
                                    on the provided material. Use with caution!";
                ui.colored_label(egui::Color32::from_rgb(255, 200, 80), warning);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests construct `AudioBatchApp` directly and drive
    // `handle_messages()` via its own mpsc channel -- no `eframe::run_native`,
    // no window, no rendering. As a `#[cfg(test)] mod` nested inside `app`,
    // this can see all of `AudioBatchApp`'s private fields without any
    // visibility changes to the struct itself.

    fn test_app() -> AudioBatchApp {
        AudioBatchApp::new(PathBuf::from("ffmpeg"))
    }

    // -----------------------------------------------------------------------
    // same_folder
    // -----------------------------------------------------------------------

    #[test]
    fn same_folder_true_for_identical_nonexistent_paths() {
        // Neither path exists, so canonicalize() fails on both and this
        // falls back to raw equality.
        let p = PathBuf::from("/nonexistent/vocan-test-path-xyz");
        assert!(same_folder(&p, &p));
    }

    #[test]
    fn same_folder_false_for_different_paths() {
        let a = PathBuf::from("/nonexistent/vocan-test-a");
        let b = PathBuf::from("/nonexistent/vocan-test-b");
        assert!(!same_folder(&a, &b));
    }

    #[test]
    fn same_folder_true_for_equivalent_paths_via_canonicalize() {
        // Two textually different but equivalent paths to the same existing
        // directory must canonicalize to the same target, even though a raw
        // PathBuf comparison says they differ. Rust's `Path` equality already
        // normalizes away a bare trailing "." (CurDir), so that alone isn't
        // a valid textual-difference example -- ".." (ParentDir) components
        // are kept as real path segments, so routing through a real sibling
        // directory and back up is what actually needs canonicalize to
        // resolve.
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("subdir");
        let sibling = root.path().join("sibling");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&sibling).unwrap();

        let direct = target.clone();
        let via_sibling = sibling.join("..").join("subdir");

        assert_ne!(
            direct, via_sibling,
            "test setup: paths should differ textually"
        );
        assert!(same_folder(&direct, &via_sibling));
    }

    #[cfg(windows)]
    #[test]
    fn same_folder_ignores_case_when_neither_path_can_be_canonicalized() {
        // On Windows the filesystem is case-insensitive, so these name the same
        // folder. Neither exists, so `canonicalize` fails on both and the
        // comparison falls back to raw `==` -- which used to be case-sensitive
        // and let this pair straight through the overwrite guard.
        let a = PathBuf::from(r"C:\nonexistent\VOCAN-Case-Test");
        let b = PathBuf::from(r"C:\nonexistent\vocan-case-test");
        assert_ne!(a, b, "test setup: the paths must differ textually");
        assert!(same_folder(&a, &b));
    }

    // -----------------------------------------------------------------------
    // validate_folders
    // -----------------------------------------------------------------------

    #[test]
    fn validate_folders_rejects_a_nested_output_folder() {
        // Survives one run, then poisons the next: the scan picks the previous
        // run's results up as source files and nests them deeper each time.
        let root = tempfile::tempdir().unwrap();
        let input = root.path().to_path_buf();
        let output = input.join("out");

        let err = validate_folders(&input, &output).expect_err("nested output must be rejected");
        assert!(err.contains("inside the source folder"), "got: {err}");
    }

    #[test]
    fn validate_folders_rejects_a_missing_source_folder() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");
        let output = root.path().join("out");
        let err = validate_folders(&missing, &output).expect_err("missing source must be rejected");
        assert!(err.contains("does not exist"), "got: {err}");
        assert!(
            !output.exists(),
            "a rejected source must not leave an output folder behind"
        );
    }

    #[test]
    fn validate_folders_accepts_a_sibling_output_and_creates_it() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("in");
        let output = root.path().join("out");
        std::fs::create_dir(&input).unwrap();

        validate_folders(&input, &output).expect("sibling folders are fine");
        assert!(
            output.is_dir(),
            "the output folder should have been created"
        );
    }

    #[test]
    fn validate_folders_accepts_a_source_nested_inside_the_output() {
        // The reverse nesting is harmless: outputs land beside the source tree,
        // never inside the part being scanned, so no run feeds on its own
        // results. Rejecting it would block a legitimate layout.
        let root = tempfile::tempdir().unwrap();
        let output = root.path().to_path_buf();
        let input = output.join("src");
        std::fs::create_dir(&input).unwrap();

        assert!(validate_folders(&input, &output).is_ok());
    }

    // -----------------------------------------------------------------------
    // start_processing: source/output folder collision guard
    // -----------------------------------------------------------------------

    #[test]
    fn start_processing_refuses_when_output_equals_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app();
        app.input_dir = dir.path().display().to_string();
        app.output_dir = dir.path().display().to_string();

        app.start_processing(egui::Context::default());

        assert!(
            !app.is_processing,
            "must not start processing when input == output"
        );
        assert_eq!(app.logs.len(), 1);
        assert!(app.logs[0].starts_with("[ERROR]"));
    }

    // -----------------------------------------------------------------------
    // CompletionGuard: a worker thread must always report a terminal message
    // -----------------------------------------------------------------------

    #[test]
    fn completion_guard_delivers_finished_when_dropped_without_disarm() {
        let mut app = test_app();
        app.is_processing = true;
        drop(CompletionGuard::new(
            app.sender.clone(),
            egui::Context::default(),
        ));
        app.handle_messages();
        assert!(
            !app.is_processing,
            "a guard dropped without disarm must deliver Finished"
        );
        assert!(app.logs.iter().any(|l| l.starts_with("[ERROR]")));
    }

    #[test]
    fn completion_guard_stays_silent_after_disarm() {
        let mut app = test_app();
        app.is_processing = true;
        let mut guard = CompletionGuard::new(app.sender.clone(), egui::Context::default());
        guard.disarm();
        drop(guard);
        app.handle_messages();
        assert!(
            app.is_processing,
            "a disarmed guard must not send anything of its own"
        );
        assert!(app.logs.is_empty());
    }

    #[test]
    fn completion_guard_unsticks_the_ui_when_the_worker_thread_panics() {
        // The actual regression: rayon re-raises a worker panic in the thread
        // that called par_iter().for_each, outside the per-file catch_unwind.
        // Before the guard, that killed the worker with no terminal message and
        // left is_processing == true for the rest of the session -- settings
        // disabled, START greyed out, Stop unable to clear it.
        //
        // The panic message this prints to stderr during the run is expected.
        let mut app = test_app();
        app.is_processing = true;
        let tx = app.sender.clone();
        let handle = thread::spawn(move || {
            let _completion = CompletionGuard::new(tx, egui::Context::default());
            panic!("simulated worker panic");
        });
        assert!(handle.join().is_err(), "test setup: the thread must panic");

        app.handle_messages();
        assert!(
            !app.is_processing,
            "UI must recover after a worker thread unwinds"
        );
    }

    // -----------------------------------------------------------------------
    // start_processing: re-entrancy
    // -----------------------------------------------------------------------

    #[test]
    fn start_processing_is_a_no_op_while_a_run_is_already_in_flight() {
        for (processing, analyzing) in [(true, false), (false, true)] {
            let dir_in = tempfile::tempdir().unwrap();
            let dir_out = tempfile::tempdir().unwrap();
            let mut app = test_app();
            app.input_dir = dir_in.path().display().to_string();
            app.output_dir = dir_out.path().display().to_string();
            app.push_log("pre-existing".to_string());
            app.is_processing = processing;
            app.is_analyzing = analyzing;

            app.start_processing(egui::Context::default());

            // A run that actually started would have cleared the log first.
            assert_eq!(
                app.logs.len(),
                1,
                "re-entrant call (processing={processing}, analyzing={analyzing}) \
                 must not clear state or spawn a second run"
            );
        }
    }

    #[test]
    fn handle_messages_appends_log_line() {
        let mut app = test_app();
        app.sender.send(AppMsg::Log("hello".to_string())).unwrap();
        app.handle_messages();
        assert_eq!(app.logs, vec!["hello".to_string()]);
    }

    #[test]
    fn handle_messages_prefixes_error_with_error_tag() {
        let mut app = test_app();
        app.sender.send(AppMsg::Error("boom".to_string())).unwrap();
        app.handle_messages();
        assert_eq!(app.logs, vec!["[ERROR] boom".to_string()]);
    }

    #[test]
    fn handle_messages_updates_progress_counters() {
        let mut app = test_app();
        app.sender.send(AppMsg::Progress(3, 10)).unwrap();
        app.handle_messages();
        assert_eq!(app.current_progress, 3);
        assert_eq!(app.total_files, 10);
    }

    #[test]
    fn handle_messages_finished_clears_processing_and_analyzing_flags() {
        let mut app = test_app();
        app.is_processing = true;
        app.is_analyzing = true;
        app.sender.send(AppMsg::Finished).unwrap();
        app.handle_messages();
        assert!(!app.is_processing);
        assert!(!app.is_analyzing);
    }

    #[test]
    fn handle_messages_stopped_clears_processing_and_analyzing_flags() {
        let mut app = test_app();
        app.is_processing = true;
        app.is_analyzing = true;
        app.sender.send(AppMsg::Stopped).unwrap();
        app.handle_messages();
        assert!(!app.is_processing);
        assert!(!app.is_analyzing);
    }

    #[test]
    fn handle_messages_analysis_result_sets_average_lufs() {
        let mut app = test_app();
        app.sender.send(AppMsg::AnalysisResult(-16.3)).unwrap();
        app.handle_messages();
        assert_eq!(app.average_lufs, Some(-16.3));
    }

    #[test]
    fn handle_messages_drains_multiple_queued_messages_in_order() {
        let mut app = test_app();
        app.sender.send(AppMsg::Log("a".to_string())).unwrap();
        app.sender.send(AppMsg::Progress(1, 2)).unwrap();
        app.sender.send(AppMsg::Log("b".to_string())).unwrap();
        app.handle_messages();
        assert_eq!(app.logs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(app.current_progress, 1);
        assert_eq!(app.total_files, 2);
    }

    #[test]
    fn handle_messages_on_empty_channel_is_a_no_op() {
        let mut app = test_app();
        app.handle_messages();
        assert!(app.logs.is_empty());
        assert_eq!(app.current_progress, 0);
        assert_eq!(app.total_files, 0);
    }

    #[test]
    fn push_log_evicts_oldest_lines_past_the_cap() {
        let mut app = test_app();
        for i in 0..MAX_LOG_LINES + 10 {
            app.push_log(format!("line {i}"));
        }
        assert_eq!(app.logs.len(), MAX_LOG_LINES);
        // The oldest 10 were dropped, the newest is still present.
        assert_eq!(app.logs.front().unwrap(), "line 10");
        assert_eq!(
            app.logs.back().unwrap(),
            &format!("line {}", MAX_LOG_LINES + 9)
        );
    }

    #[test]
    fn push_log_truncates_an_over_long_line() {
        let mut app = test_app();
        app.push_log("x".repeat(MAX_LOG_LINE_CHARS * 3));
        let line = app.logs.front().unwrap();
        assert!(line.ends_with("... [truncated]"));
        assert_eq!(
            line.chars().count(),
            MAX_LOG_LINE_CHARS + "... [truncated]".len()
        );
    }

    #[test]
    fn push_log_truncates_on_a_char_boundary() {
        // Truncating by byte offset would panic mid-codepoint; each 'ą' is two
        // bytes, so a byte-based cut at MAX_LOG_LINE_CHARS would land inside one.
        let mut app = test_app();
        app.push_log("ą".repeat(MAX_LOG_LINE_CHARS * 2));
        let line = app.logs.front().unwrap();
        assert!(line.starts_with('ą'));
        assert!(line.ends_with("... [truncated]"));
    }

    #[test]
    fn processing_options_default_matches_the_ui_defaults() {
        // `ProcessingOptions::default()` and `AudioBatchApp::new` declare the
        // same defaults in two places. Nothing else notices them drifting, and
        // the drift is silent where it matters most: a wrong
        // `target_peak_dbfs` or `bitrate_kbps` in the Default impl would leave
        // every test that builds options with `..Default::default()` passing
        // while quietly exercising a different configuration.
        //
        // Destructured exhaustively on purpose -- adding a field to
        // ProcessingOptions without deciding its default breaks this test to
        // compile, which is the point.
        let ProcessingOptions {
            target_lufs,
            target_peak_dbfs,
            automixer,
            automixer_spectral_gate,
            automixer_nn_dereverb,
            automixer_dfn3_dereverb,
            automixer_dfn3_mix,
            automixer_dfn3_postfilter,
            automixer_expander,
            automixer_expander_safety_pct,
            automixer_expander_reduction_profile,
            output_format,
            bitrate_kbps,
            log: _, // wired to the live UI channel, deliberately None by default
        } = test_app().processing_options();

        let d = ProcessingOptions::default();
        assert_eq!(target_lufs, d.target_lufs);
        assert_eq!(target_peak_dbfs, d.target_peak_dbfs);
        assert_eq!(automixer, d.automixer);
        assert_eq!(automixer_spectral_gate, d.automixer_spectral_gate);
        assert_eq!(automixer_nn_dereverb, d.automixer_nn_dereverb);
        assert_eq!(automixer_dfn3_dereverb, d.automixer_dfn3_dereverb);
        assert_eq!(automixer_dfn3_mix, d.automixer_dfn3_mix);
        assert_eq!(automixer_dfn3_postfilter, d.automixer_dfn3_postfilter);
        assert_eq!(automixer_expander, d.automixer_expander);
        assert_eq!(
            automixer_expander_safety_pct,
            d.automixer_expander_safety_pct
        );
        assert_eq!(
            automixer_expander_reduction_profile,
            d.automixer_expander_reduction_profile
        );
        assert_eq!(output_format, d.output_format);
        assert_eq!(bitrate_kbps, d.bitrate_kbps);
        assert!(ProcessingOptions::default().log.is_none());
    }

    #[test]
    fn new_app_has_sane_defaults() {
        let app = test_app();
        assert!(!app.normalize_volume);
        assert!(!app.automixer);
        assert!(!app.is_processing);
        assert!(!app.is_analyzing);
        assert_eq!(app.average_lufs, None);
        assert_eq!(app.output_format, OutputFormat::AdpcmWav);
        assert_eq!(
            app.automixer_expander_reduction_profile,
            ReductionProfile::Recommended
        );
    }
}
