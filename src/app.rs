use eframe::egui;
use eframe::egui::{Align, Layout, RichText, TextStyle, Vec2};
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
use crate::theme;
// Aliased: every UI function here already has a local binding called `ui`, and
// `widgets::card(ui, ..)` reads better than the (legal, but confusing)
// `ui::card(ui, ..)`.
use crate::types::{AppMsg, NormResult, OutputFormat, ProcessingOptions, ReductionProfile};
use crate::ui as widgets;

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
    /// Trims leading and trailing silence from every take.
    ///
    /// Not one of the automixer modules above and not gated by
    /// [`Self::automixer`]: it is a single FFmpeg filter folded into a chain
    /// the pipeline already runs, so it works on its own and the only thing
    /// VOCAN insists on around it is the format conversion.
    trim_silence: bool,
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
    /// Which section the content pane is showing. Pure view state: no
    /// processing parameter depends on it.
    section: Section,
    /// Set when a run starts, so that the pane can switch itself to the log
    /// once and then leave the user free to navigate away without being
    /// yanked back on the next message.
    followed_run: bool,
    /// How many retained log lines are errors.
    ///
    /// Kept as a running count rather than recomputed per frame: the rail
    /// shows it on every repaint, and scanning up to [`MAX_LOG_LINES`] lines
    /// sixty times a second to render one small number is exactly the kind of
    /// per-frame work that made the old log pane crawl.
    error_count: usize,
}

/// The sections of the navigation rail.
///
/// `Files` holds both ends of the pipeline -- the folders and the output
/// format -- because separately they were two- and three-control sections
/// leaving most of the pane empty. Clean-up then loudness is the order the
/// audio actually travels; the old single-column layout listed loudness
/// before the automixer, which is the reverse of what the pipeline does.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Section {
    #[default]
    Files,
    CleanUp,
    Loudness,
    Logs,
}

impl Section {
    /// Heading shown at the top of the content pane.
    pub fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::CleanUp => "Clean up",
            Self::Loudness => "Loudness",
            Self::Logs => "Logs",
        }
    }

    /// One-line explanation under the heading. The pane has the room for it,
    /// which is the main thing this layout buys over a single column.
    pub fn description(self) -> &'static str {
        match self {
            Self::Files => {
                "Every audio file under the source folder is processed, and the folder tree is \
                 recreated inside the output folder in the format chosen here. Trimming the \
                 silence off each take is here too \u{2014} it needs nothing from Clean up."
            }
            Self::CleanUp => {
                "De-ess \u{2192} dereverb \u{2192} expand \u{2192} denoise \u{2192} voice EQ \
                 \u{2192} compress. The whole chain runs before loudness normalization."
            }
            Self::Loudness => {
                "Two-pass EBU R128, with a padded measurement for files under 3 seconds and \
                 peak normalization as a last resort."
            }
            Self::Logs => "One line per file, plus anything that went wrong.",
        }
    }
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
            trim_silence: false,
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
            section: Section::default(),
            followed_run: false,
            error_count: 0,
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
            if let Some(evicted) = self.logs.pop_front() {
                if evicted.starts_with("[ERROR]") {
                    self.error_count -= 1;
                }
            }
        }
        if text.starts_with("[ERROR]") {
            self.error_count += 1;
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
            trim_silence: self.trim_silence,
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
//
// Layout: a fixed navigation rail on the left, one section at a time in the
// content pane, and an action bar pinned under the pane.
//
// The obvious risk of a rail is that settings you cannot see are settings you
// forget to check before committing to a long batch. Two things answer that,
// and both are load-bearing rather than decorative: every rail item carries a
// live summary of its own section, and the action bar carries the whole recipe
// directly above the button that acts on it.

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

        // Show the log when a run starts -- but only once per run. Re-asserting
        // it every frame would trap the user in the log pane for the whole
        // batch, unable to look at the settings the run is using.
        if self.is_processing || self.is_analyzing {
            if !self.followed_run {
                self.section = Section::Logs;
                self.followed_run = true;
            }
        } else {
            self.followed_run = false;
        }

        // The rail is registered first, so every panel after it is laid out in
        // the width that remains. That is what keeps the action bar inside the
        // content pane instead of running the full width of the window under
        // the rail.
        let rail = egui::SidePanel::left("rail")
            .exact_width(theme::RAIL_W)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin {
                        left: 12.0,
                        right: 12.0,
                        top: 16.0,
                        bottom: 14.0,
                    }),
            )
            .show(ctx, |ui| self.ui_rail(ui));

        // egui's own panel separator follows the widget stroke, which the theme
        // has set for controls rather than chrome. One explicit hairline keeps
        // the rail's edge independent of that.
        //
        // Taken from the panel's own rect, never from `RAIL_W`: `exact_width`
        // sizes the panel's *content*, and the frame's 12px margins are added
        // outside that, so the panel is 24px wider than the number handed to
        // it. Painting the hairline at `RAIL_W` put it 24px inside the rail,
        // which made every nav row look like it spilled across the divider.
        let screen = ctx.screen_rect();
        let rail_edge = rail.response.rect.right();
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("rail_edge"),
        ))
        .rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rail_edge - 1.0, screen.top()),
                Vec2::new(1.0, screen.height()),
            ),
            egui::Rounding::ZERO,
            theme::LINE,
        );

        egui::TopBottomPanel::bottom("action")
            .frame(
                egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin {
                        left: 22.0,
                        right: 22.0,
                        top: 13.0,
                        bottom: 14.0,
                    }),
            )
            .show(ctx, |ui| self.ui_action_bar(ui, ctx));

        egui::CentralPanel::default()
            // `Frame::none()` carries no margins at all, so without this the
            // pane's cards run flush into the window edge and wrapping text
            // has nothing to wrap against.
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 22.0,
                        right: 22.0,
                        top: 0.0,
                        bottom: 0.0,
                    }),
            )
            .show(ctx, |ui| {
                self.ui_pane_header(ui);
                let section = self.section;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .id_source("pane_scroll")
                    .show(ui, |ui| {
                        // Settings are visible but not editable while a batch
                        // runs. The log is exempt: it is the one thing the user
                        // has any reason to interact with mid-run.
                        ui.add_enabled_ui(!self.is_processing || section == Section::Logs, |ui| {
                            match section {
                                Section::Files => self.ui_files(ui),
                                Section::CleanUp => self.ui_cleanup(ui),
                                Section::Loudness => self.ui_loudness(ui, ctx),
                                Section::Logs => self.ui_logs(ui),
                            }
                        });
                        ui.add_space(14.0);
                    });
            });
    }
}

// ---------------------------------------------------------------------------
// Navigation rail
// ---------------------------------------------------------------------------

impl AudioBatchApp {
    fn ui_rail(&mut self, ui: &mut egui::Ui) {
        widgets::brand(ui);
        ui.add_space(16.0);

        widgets::rail_group(ui, "SETUP");
        ui.add_space(4.0);

        ui.scope(|ui| {
            // Nav rows carry their own 42px height; the default 8px gap between
            // them would read as separate buttons rather than one list.
            ui.spacing_mut().item_spacing.y = 2.0;

            let (text, color) = self.summary_files();
            if widgets::nav_item(
                ui,
                self.section == Section::Files,
                widgets::Icon::Folder,
                "Files",
                &text,
                color,
            )
            .clicked()
            {
                self.section = Section::Files;
            }

            let (text, color) = self.summary_cleanup();
            if widgets::nav_item(
                ui,
                self.section == Section::CleanUp,
                widgets::Icon::Wave,
                "Clean up",
                &text,
                color,
            )
            .clicked()
            {
                self.section = Section::CleanUp;
            }

            let (text, color) = self.summary_loudness();
            if widgets::nav_item(
                ui,
                self.section == Section::Loudness,
                widgets::Icon::Bars,
                "Loudness",
                &text,
                color,
            )
            .clicked()
            {
                self.section = Section::Loudness;
            }
        });

        ui.add_space(16.0);
        widgets::rail_group(ui, "RUN");
        ui.add_space(4.0);

        let (text, color) = self.summary_logs();
        if widgets::nav_item(
            ui,
            self.section == Section::Logs,
            widgets::Icon::List,
            "Logs",
            &text,
            color,
        )
        .clicked()
        {
            self.section = Section::Logs;
        }

        // FFmpeg status sits at the bottom of the rail. It used to be a red
        // paragraph above the start button, which is both the busiest part of
        // the window and the last place you look before committing to a run.
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.add_space(2.0);
            match &self.ffmpeg_error {
                None => {
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        let (dot, _) =
                            ui.allocate_exact_size(Vec2::splat(6.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot.center(), 3.0, theme::GREEN);
                        ui.add_space(1.0);
                        ui.label(RichText::new("FFmpeg ready").size(11.0).color(theme::GREEN));
                    });
                }
                Some(err) => {
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        let (dot, _) =
                            ui.allocate_exact_size(Vec2::splat(6.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot.center(), 3.0, theme::RED);
                        ui.add_space(1.0);
                        ui.label(RichText::new("FFmpeg missing").size(11.0).color(theme::RED))
                            .on_hover_text(err);
                    });
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Rail summaries
    // -----------------------------------------------------------------------
    //
    // Each returns the text and colour for one rail row. Amber means "this
    // will stop the run"; the muted grey is a setting at rest.

    /// One row now stands for both the folders and the output format, so it
    /// leads with whatever would stop a run: missing folders first, the
    /// format only once there is nothing to warn about.
    fn summary_files(&self) -> (String, egui::Color32) {
        if self.input_dir.is_empty() || self.output_dir.is_empty() {
            return ("not set".to_owned(), theme::AMBER);
        }
        let format = format_short(self.output_format, self.bitrate_kbps);
        if self.trim_silence {
            // The summary is clipped from the left when it outruns the gap
            // beside the label, so the trim goes last: "Files" is a short
            // label and leaves room, but if anything is ever lost here it
            // should be the format, which the pane also states in full.
            return (format!("{format} + trim"), theme::TXT3);
        }
        (format, theme::TXT3)
    }

    fn summary_cleanup(&self) -> (String, egui::Color32) {
        if !self.automixer {
            return ("off".to_owned(), theme::TXT3);
        }
        // Voice EQ, the de-esser and the post chain are unconditional once the
        // automixer is on, so the count is "what you switched on" plus that
        // fixed stage -- the same set the recipe line in the action bar names.
        let n = 1
            + usize::from(self.automixer_dfn3_dereverb)
            + usize::from(self.automixer_expander)
            + usize::from(self.automixer_spectral_gate || self.automixer_nn_dereverb);
        (format!("{n} stages"), theme::TXT3)
    }

    fn summary_loudness(&self) -> (String, egui::Color32) {
        if self.normalize_volume {
            (format!("{:.1}", self.target_lufs), theme::TXT3)
        } else {
            ("off".to_owned(), theme::TXT3)
        }
    }

    fn summary_logs(&self) -> (String, egui::Color32) {
        if self.error_count > 0 {
            (
                format!(
                    "{} error{}",
                    self.error_count,
                    if self.error_count == 1 { "" } else { "s" }
                ),
                theme::RED,
            )
        } else if self.logs.is_empty() {
            (String::new(), theme::TXT3)
        } else {
            (self.logs.len().to_string(), theme::TXT3)
        }
    }

    /// The whole configuration on one line, for the action bar.
    ///
    /// This is the counterweight to showing one section at a time: whatever
    /// section is open, the settings that the button is about to act on are
    /// spelled out immediately above it.
    fn recipe(&self) -> String {
        let mut parts = vec![format_short(self.output_format, self.bitrate_kbps)];

        parts.push(if self.normalize_volume {
            format!("{:.1} LUFS", self.target_lufs)
        } else {
            "no normalization".to_owned()
        });

        if self.trim_silence {
            parts.push("trim silence".to_owned());
        }

        if self.automixer {
            let mut modules: Vec<&str> = Vec::new();
            if self.automixer_dfn3_dereverb {
                modules.push("dereverb");
            }
            if self.automixer_expander {
                modules.push("expander");
            }
            if self.automixer_spectral_gate {
                modules.push("spectral gate");
            }
            if self.automixer_nn_dereverb {
                modules.push("nnnoiseless");
            }
            modules.push("voice EQ");
            parts.push(format!("clean up: {}", modules.join(", ")));
        } else {
            parts.push("no clean-up".to_owned());
        }

        parts.join("   \u{b7}   ")
    }
}

/// A short label for the rail and the recipe line.
///
/// [`OutputFormat::label`] is written for the dropdown, where there is room to
/// explain each option; at 10.5px in a 198px rail there is not.
fn format_short(format: OutputFormat, bitrate: u32) -> String {
    match format {
        OutputFormat::AdpcmWav => "ADPCM".to_owned(),
        OutputFormat::Pcm16Wav => "PCM 16".to_owned(),
        OutputFormat::Pcm24Wav => "PCM 24".to_owned(),
        OutputFormat::Flac => "FLAC".to_owned(),
        OutputFormat::Mp3 => format!("MP3 {bitrate}k"),
        OutputFormat::Ogg => format!("OGG {bitrate}k"),
    }
}

// ---------------------------------------------------------------------------
// Content pane
// ---------------------------------------------------------------------------

impl AudioBatchApp {
    /// Title, description, and the section's master switch where it has one.
    fn ui_pane_header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.heading(RichText::new(self.section.title()).color(theme::TXT));

            let section = self.section;
            let running = self.is_processing;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!running, |ui| match section {
                    Section::CleanUp => {
                        widgets::toggle(ui, &mut self.automixer);
                    }
                    Section::Loudness => {
                        widgets::toggle(ui, &mut self.normalize_volume);
                    }
                    _ => {}
                });
            });
        });

        ui.add_space(7.0);
        ui.add(
            egui::Label::new(
                RichText::new(self.section.description())
                    .size(12.0)
                    .color(theme::TXT3),
            )
            .wrap(true),
        );
        ui.add_space(15.0);
        widgets::divider(ui);
        ui.add_space(15.0);
    }

    // -----------------------------------------------------------------------

    /// Both ends of the pipeline: the folders the batch reads and writes, and
    /// the format it writes in. Kept in one section because each was two or
    /// three controls on its own, which left most of the pane empty.
    fn ui_files(&mut self, ui: &mut egui::Ui) {
        widgets::card(ui, |ui| {
            ui.label(RichText::new("Folders").color(theme::TXT));
            ui.add_space(11.0);
            let w = label_col(
                ui,
                &["Source", "Output"],
                TextStyle::Body.resolve(ui.style()),
            );
            path_row(ui, "Source", w, &mut self.input_dir);
            ui.add_space(10.0);
            path_row(ui, "Output", w, &mut self.output_dir);
        });

        if !self.input_dir.is_empty() && self.input_dir == self.output_dir {
            ui.add_space(11.0);
            widgets::notice(
                ui,
                "The output folder is the source folder. Point them at different folders \
                 \u{2014} writing a file while FFmpeg is reading it can destroy the original.",
            );
        }

        ui.add_space(12.0);

        widgets::card(ui, |ui| {
            ui.label(RichText::new("Output format").color(theme::TXT));
            ui.add_space(11.0);
            let w = label_col(
                ui,
                &["Format", "Bitrate"],
                TextStyle::Body.resolve(ui.style()),
            );
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(w, 30.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.label(RichText::new("Format").color(theme::TXT2));
                    },
                );
                let combo_w = ui.available_width();
                egui::ComboBox::from_id_source("output_format")
                    .width(combo_w - 8.0)
                    .selected_text(self.output_format.label())
                    .show_ui(ui, |ui| {
                        for &fmt in OutputFormat::all() {
                            ui.selectable_value(&mut self.output_format, fmt, fmt.label());
                        }
                    });
            });

            if self.output_format.needs_bitrate() {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(w, 30.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.label(RichText::new("Bitrate").color(theme::TXT2));
                        },
                    );
                    let combo_w = ui.available_width();
                    egui::ComboBox::from_id_source("bitrate_combo")
                        .width(combo_w - 8.0)
                        .selected_text(format!("{} kbps", self.bitrate_kbps))
                        .show_ui(ui, |ui| {
                            for &b in &[36, 48, 64, 128, 256, 320] {
                                ui.selectable_value(&mut self.bitrate_kbps, b, format!("{b} kbps"));
                            }
                        });
                });
            }

            if self.output_format == OutputFormat::AdpcmWav {
                ui.add_space(9.0);
                widgets::hint(ui, "Suggested for video game voice-over");
            }
        });

        if self.output_format.needs_bitrate() && self.bitrate_kbps <= 48 {
            ui.add_space(12.0);
            widgets::notice(
                ui,
                "Highest compression, requires a verification for quality.",
            );
        }

        ui.add_space(12.0);

        // The trim lives here rather than in Clean up because it is not part of
        // that chain and is not governed by that section's master switch --
        // parked under a toggle it ignores, it would read as one more module
        // that goes quiet when the toggle is off. Here it sits with the other
        // things VOCAN does to every file no matter what else is configured.
        //
        // In the pipeline it still runs first, ahead of everything in Clean up.
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::check(ui, &mut self.trim_silence, "Trim silence");
                ui.label(RichText::new("start and end").size(11.5).color(theme::TXT3));
            });
            indented_hint(
                ui,
                "Cuts the lead-in and the tail off every take at \u{2212}45 dB. Pauses inside \
                 the line are kept. Needs nothing from Clean up.",
            );
        });
    }

    // -----------------------------------------------------------------------

    fn ui_cleanup(&mut self, ui: &mut egui::Ui) {
        // One column shared by every row in the section, so the sliders and the
        // dropdown line up across separate cards.
        let w = label_col(
            ui,
            &["Mix", "Safety margin", "Reduction"],
            egui::FontId::new(12.5, egui::FontFamily::Proportional),
        );
        // The master switch lives in the pane header. With it off, the modules
        // stay visible but inert -- the chain is still worth reading when it is
        // not going to run.
        let on = self.automixer;
        ui.add_enabled_ui(on, |ui| {
            widgets::card(ui, |ui| {
                ui.horizontal(|ui| {
                    widgets::check(ui, &mut self.automixer_dfn3_dereverb, "Dereverb");
                    ui.label(
                        RichText::new("DeepFilterNet3")
                            .size(11.5)
                            .color(theme::TXT3),
                    );
                });
                indented_hint(
                    ui,
                    "Removes room reflections. Also reduces broadband noise.",
                );
                if self.automixer_dfn3_dereverb {
                    ui.add_space(11.0);
                    let readout = format!("{:.0}%", self.automixer_dfn3_mix);
                    slider_row(
                        ui,
                        "Mix",
                        w,
                        &mut self.automixer_dfn3_mix,
                        0.0..=100.0,
                        &readout,
                    );
                    ui.add_space(9.0);
                    ui.horizontal(|ui| {
                        ui.add_space(27.0);
                        widgets::check_colored(
                            ui,
                            &mut self.automixer_dfn3_postfilter,
                            "Post-filter (aggressive)",
                            theme::TXT2,
                        );
                    });
                }
            });

            ui.add_space(12.0);

            widgets::card(ui, |ui| {
                ui.horizontal(|ui| {
                    widgets::check(ui, &mut self.automixer_expander, "Downward expander");
                    ui.label(RichText::new("noise floor").size(11.5).color(theme::TXT3));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::tag(ui, "compute heavy");
                    });
                });
                indented_hint(
                    ui,
                    "Pushes the gaps between words down, without gating them to silence.",
                );
                if self.automixer_expander {
                    ui.add_space(11.0);
                    let readout = format!("{:.0}%", self.automixer_expander_safety_pct);
                    slider_row(
                        ui,
                        "Safety margin",
                        w,
                        &mut self.automixer_expander_safety_pct,
                        0.0..=100.0,
                        &readout,
                    )
                    .on_hover_text(
                        "Higher = safer, touches less material.\n\
                         50% is a good starting point.\n\
                         The threshold sits below the detected noise floor\n\
                         by a margin derived from this setting.",
                    );

                    ui.add_space(9.0);
                    ui.horizontal(|ui| {
                        ui.add_space(27.0);
                        ui.allocate_ui_with_layout(
                            Vec2::new(w, 22.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.label(RichText::new("Reduction").size(12.5).color(theme::TXT2));
                            },
                        );
                        let combo_w = ui.available_width();
                        egui::ComboBox::from_id_source("reduction_profile")
                            .width(combo_w - 8.0)
                            .selected_text(self.automixer_expander_reduction_profile.label())
                            .show_ui(ui, |ui| {
                                for &profile in ReductionProfile::all() {
                                    ui.selectable_value(
                                        &mut self.automixer_expander_reduction_profile,
                                        profile,
                                        profile.label(),
                                    );
                                }
                            });
                    });

                    if self.automixer_expander_reduction_profile == ReductionProfile::Max {
                        ui.add_space(10.0);
                        widgets::notice(
                            ui,
                            "MAX (-32 dB) is aggressive \u{2014} can sound like a hard gate on \
                             RMS-detected material below an already-conservative threshold.",
                        );
                    }
                }
            });

            ui.add_space(12.0);

            widgets::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Noise reduction").color(theme::TXT));
                    ui.label(RichText::new("pick one").size(11.5).color(theme::TXT3));
                });
                indented_hint_flush(
                    ui,
                    "Two algorithms competing for the same slot in the chain.",
                );
                ui.add_space(11.0);

                // The two flags are stored separately but only one may be set.
                // As a segmented control the exclusivity is structural: there
                // is no state the widget can produce that violates it.
                let selected = if self.automixer_spectral_gate {
                    1
                } else if self.automixer_nn_dereverb {
                    2
                } else {
                    0
                };
                if let Some(i) =
                    widgets::segmented(ui, selected, &["Off", "Spectral gate", "nnnoiseless"])
                {
                    self.automixer_spectral_gate = i == 1;
                    self.automixer_nn_dereverb = i == 2;
                }
            });

            ui.add_space(12.0);

            widgets::card(ui, |ui| {
                ui.label(RichText::new("Always on").color(theme::TXT2));
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Voice EQ at 50% strength \u{b7} de-esser \u{b7} post EQ and \
                             compressor with make-up gain.\nStereo sources are downmixed to mono.",
                        )
                        .size(11.5)
                        .color(theme::TXT3),
                    )
                    .wrap(true),
                );
            });

            ui.add_space(12.0);
            widgets::notice(
                ui,
                "Attention! There is no way to create a universal mixing tool. This is the \
                 closest I can think of to a universal mixing chain without doing proper mixing, \
                 but the results may drastically vary based on the provided material. Use with \
                 caution!",
            );
        });
    }

    // -----------------------------------------------------------------------

    fn ui_loudness(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Measure a folder").color(theme::TXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // The enclosing pane is already disabled while
                    // `is_processing`; guarding `is_analyzing` here as well
                    // stops a second click starting a re-entrant analysis while
                    // one is in flight -- both share cancel_flag, logs and
                    // progress state.
                    ui.add_enabled_ui(!self.is_analyzing, |ui| {
                        if widgets::small_button(ui, "Analyze folder loudness\u{2026}")
                            .on_hover_text("Select a folder to check its average loudness level")
                            .clicked()
                        {
                            self.start_analysis(ctx.clone());
                        }
                    });
                });
            });
            indented_hint_flush(
                ui,
                "Checks the average level of the files you already have, so you can pick a \
                 target that is not a guess.",
            );

            if let Some(avg) = self.average_lufs {
                ui.add_space(11.0);
                egui::Frame::none()
                    .fill(theme::INPUT)
                    .stroke(egui::Stroke::new(1.0, theme::LINE))
                    .rounding(egui::Rounding::same(theme::R_CTRL))
                    .inner_margin(egui::Margin::symmetric(11.0, 9.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Average level of your files")
                                    .size(12.5)
                                    .color(theme::TXT2),
                            );
                            ui.label(
                                RichText::new(format!("{avg:.2} LUFS"))
                                    .monospace()
                                    .strong()
                                    .color(theme::AMBER),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::small_button(ui, "Set as target").clicked() {
                                    self.target_lufs = avg.round();
                                }
                            });
                        });
                    });
            }
        });

        ui.add_space(12.0);

        let on = self.normalize_volume;
        ui.add_enabled_ui(on, |ui| {
            widgets::card(ui, |ui| {
                let w = label_col(
                    ui,
                    &["Target LUFS-I", "Target peak"],
                    egui::FontId::new(12.5, egui::FontFamily::Proportional),
                );
                let readout = format!("{:.1}", self.target_lufs);
                slider_row(
                    ui,
                    "Target LUFS-I",
                    w,
                    &mut self.target_lufs,
                    -23.0..=-6.0,
                    &readout,
                );
                indented_hint(
                    ui,
                    "EBU R128 integrated \u{b7} padded measurement below 3 s",
                );

                ui.add_space(13.0);

                let readout = format!("{:.1}", self.target_peak_dbfs);
                slider_row(
                    ui,
                    "Target peak",
                    w,
                    &mut self.target_peak_dbfs,
                    -12.0..=-1.0,
                    &readout,
                )
                .on_hover_text(
                    "Peak normalization fallback, used only when EBU R128 loudness \
                     measurement (standard or padded) fails -- typically for silent \
                     or near-silent samples.\n\
                     Recommended: -3 dBFS (safe headroom for 4-bit ADPCM).",
                );
                indented_hint(
                    ui,
                    "dBFS fallback \u{b7} used only when the R128 measurement fails",
                );
            });
        });
    }

    // -----------------------------------------------------------------------

    fn ui_logs(&mut self, ui: &mut egui::Ui) {
        if self.logs.is_empty() {
            widgets::card(ui, |ui| {
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Nothing yet \u{2014} the log fills as files are processed.")
                            .size(12.0)
                            .color(theme::TXT3),
                    );
                });
                ui.add_space(6.0);
            });
            return;
        }

        egui::Frame::none()
            .fill(theme::INPUT)
            .stroke(egui::Stroke::new(1.0, theme::LINE))
            .rounding(egui::Rounding::same(theme::R_CARD))
            .inner_margin(egui::Margin::symmetric(4.0, 8.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .id_source("log_scroll")
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for log in &self.logs {
                            log_line(ui, log);
                        }
                    });
            });
    }

    // -----------------------------------------------------------------------
    // Action bar
    // -----------------------------------------------------------------------

    fn ui_action_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.is_processing || self.is_analyzing {
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
                ui.label(RichText::new(label).strong().color(theme::TXT));
                ui.label(
                    RichText::new(format!(
                        "{} / {} files",
                        self.current_progress, self.total_files
                    ))
                    .monospace()
                    .color(theme::TXT2),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.0}%", progress * 100.0))
                            .monospace()
                            .strong()
                            .color(theme::AMBER),
                    );
                });
            });

            ui.add_space(9.0);
            ui.add(
                egui::ProgressBar::new(progress)
                    .desired_height(8.0)
                    .rounding(egui::Rounding::same(4.0))
                    .fill(theme::AMBER),
            );
            ui.add_space(11.0);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::stop_button(ui)
                    .on_hover_text(
                        "Stops now: the files still in flight are cancelled \
                         mid-conversion and their partial output is discarded",
                    )
                    .clicked()
                {
                    self.cancel_flag.store(true, Ordering::Relaxed);
                    // Terminating the running children is what makes Stop take
                    // effect *now*: the worker threads are parked in `wait()`
                    // and cannot see the flag until their subprocess returns,
                    // which for a long file with DeepFilterNet3 is minutes away.
                    crate::proc::terminate_all();
                    self.push_log("Stop requested \u{2014} finishing up...".into());
                }
            });
            return;
        }

        // The recipe line. Whatever section is open, this is what the button
        // below is about to do.
        ui.add(
            egui::Label::new(RichText::new(self.recipe()).size(11.5).color(theme::TXT3)).wrap(true),
        );
        ui.add_space(10.0);

        let can_start = !self.input_dir.is_empty() && !self.output_dir.is_empty();
        let response = widgets::go_button(ui, "START PROCESSING", can_start);
        if response.clicked() {
            self.start_processing(ctx.clone());
        }
        if !can_start {
            // A disabled button with no stated reason is the thing people file
            // bugs about. The rail already flags Source in amber; this says the
            // same thing at the point of the click.
            response.on_hover_text("Choose a source folder and an output folder first");
        }
    }
}

// ---------------------------------------------------------------------------
// Small shared pieces
// ---------------------------------------------------------------------------

/// Width of a label column, measured from the widest label that will sit in it.
///
/// These were hard-coded numbers picked to fit Segoe UI. That silently ties the
/// layout to one font: on Linux and macOS none of the Windows faces load and
/// egui falls back to its bundled Ubuntu-Light, which is wider at the same
/// size, so the longest labels ("Target LUFS-I", "Safety margin") would have
/// run into the control beside them -- a label in a horizontal layout does not
/// wrap, it overlaps. Measuring costs one text layout per row and cannot drift.
fn label_col(ui: &egui::Ui, labels: &[&str], font: egui::FontId) -> f32 {
    labels
        .iter()
        .map(|label| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), theme::TXT2)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max)
        + 12.0
}

/// A folder row: label column, field, Browse.
///
/// The label column is a fixed width rather than a plain `ui.label`, which is
/// what used to leave the two Browse buttons out of line with each other --
/// "Source: " and "Output: " are not the same number of pixels wide.
fn path_row(ui: &mut egui::Ui, label: &str, label_w: f32, value: &mut String) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(label_w, theme::CTRL_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(RichText::new(label).color(theme::TXT2));
            },
        );

        let browse_w = 78.0;
        let field_w = (ui.available_width() - browse_w - ui.spacing().item_spacing.x).max(120.0);
        // `add_sized` stretches the field to CTRL_H, but a TextEdit's margin
        // is what positions the text area inside it -- and with a zero
        // vertical margin that area starts at the top edge. `vertical_align`
        // is not enough on its own: egui paints the *hint* text at the text
        // area's top-left corner and ignores alignment entirely, so an empty
        // field showed "Not selected" riding above the label beside it.
        // Centring the area itself fixes the placeholder and the typed path
        // together. Derived from the font rather than hard-coded, so it
        // survives a change to CTRL_H or to the monospace size.
        let row_h = ui.fonts(|f| f.row_height(&TextStyle::Monospace.resolve(ui.style())));
        let pad_y = ((theme::CTRL_H - row_h) / 2.0).max(0.0);
        ui.add_sized(
            [field_w, theme::CTRL_H],
            egui::TextEdit::singleline(value)
                .font(TextStyle::Monospace)
                .margin(Vec2::new(10.0, pad_y))
                .hint_text("Not selected"),
        );

        if widgets::small_button(ui, "Browse").clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                *value = path.display().to_string();
            }
        }
    });
}

/// A slider laid out as `label | track | value`, indented under its module.
///
/// egui's own `Slider::text` puts the label after the track and the number
/// before it, which reads backwards once there is more than one slider on
/// screen: the numbers end up in a ragged column in the middle of the card.
fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    label_w: f32,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    readout: &str,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(27.0);
        ui.allocate_ui_with_layout(
            Vec2::new(label_w, 22.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(RichText::new(label).size(12.5).color(theme::TXT2));
            },
        );

        let chip_w = 56.0;
        let track_w = (ui.available_width() - chip_w - ui.spacing().item_spacing.x * 2.0).max(80.0);
        ui.spacing_mut().slider_width = track_w;
        let response = ui.add(egui::Slider::new(value, range).show_value(false));
        widgets::chip(ui, readout, chip_w);
        response
    })
    .inner
}

/// A hint indented to line up under a module's checkbox label.
fn indented_hint(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(27.0);
        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(theme::TXT3)).wrap(true));
    });
}

/// A hint with no indent, for cards whose heading is a plain label.
fn indented_hint_flush(ui: &mut egui::Ui, text: &str) {
    ui.add_space(5.0);
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(theme::TXT3)).wrap(true));
}

/// One log line: the folder dimmed, the filename bright, errors on a red wash.
///
/// The lines are full paths, and in a batch every one of them shares the same
/// long prefix. Dimming it puts the part that differs -- the filename -- in
/// front of the eye without shortening or hiding anything.
fn log_line(ui: &mut egui::Ui, text: &str) {
    let is_err = text.starts_with("[ERROR]");
    let body = |ui: &mut egui::Ui| {
        let bright = if is_err { theme::RED } else { theme::TXT2 };
        let dim = if is_err {
            theme::RED.gamma_multiply(0.62)
        } else {
            theme::TXT3
        };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            match text.rfind(['\\', '/']) {
                Some(cut) => {
                    ui.label(RichText::new(&text[..=cut]).monospace().color(dim));
                    ui.label(RichText::new(&text[cut + 1..]).monospace().color(bright));
                }
                None => {
                    ui.label(RichText::new(text).monospace().color(bright));
                }
            }
        });
    };

    if is_err {
        egui::Frame::none()
            .fill(theme::RED_WASH)
            .rounding(egui::Rounding::same(4.0))
            .inner_margin(egui::Margin::symmetric(8.0, 3.0))
            .show(ui, body);
    } else {
        egui::Frame::none()
            .inner_margin(egui::Margin::symmetric(8.0, 3.0))
            .show(ui, body);
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
            trim_silence,
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
        assert_eq!(trim_silence, d.trim_silence);
        assert_eq!(output_format, d.output_format);
        assert_eq!(bitrate_kbps, d.bitrate_kbps);
        assert!(ProcessingOptions::default().log.is_none());
    }

    #[test]
    fn new_app_has_sane_defaults() {
        let app = test_app();
        assert!(!app.normalize_volume);
        assert!(!app.automixer);
        assert!(!app.trim_silence);
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
