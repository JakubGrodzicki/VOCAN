#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod audio_effects;
mod ffmpeg;
mod processing;
mod types;

use eframe::egui;
use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    let ffmpeg_path = ffmpeg::find_ffmpeg().unwrap_or_else(|e| {
        eprintln!("{}", e);
        PathBuf::from("ffmpeg")
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([700.0, 700.0]),
        ..Default::default()
    };

    let threads = num_cpus::get().saturating_sub(1).max(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok(); // ignore if already initialized

    eframe::run_native(
        "VOCAN",
        options,
        Box::new(move |_cc| Box::new(app::AudioBatchApp::new(ffmpeg_path))),
    )
}
