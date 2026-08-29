#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
use vocan::{app, ffmpeg, theme};

fn main() -> eframe::Result<()> {
    // Deliberately not unwrapped here: with `windows_subsystem = "windows"`
    // there is no console, so reporting the failure has to happen inside the
    // GUI. `AudioBatchApp::from_ffmpeg_lookup` keeps the message and shows it.
    let ffmpeg_lookup = ffmpeg::find_ffmpeg();

    // Reclaim scratch directories abandoned by instances that were killed
    // mid-batch. Age-based and skipping our own, so a second VOCAN running
    // right now keeps its files.
    vocan::proc::sweep_scratch();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([880.0, 900.0])
            // The navigation rail is a fixed 198px, so the minimum has to
            // leave a usable pane beside it -- below roughly 700 the format
            // dropdown and the path fields start truncating.
            .with_min_inner_size([700.0, 580.0]),
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
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Box::new(app::AudioBatchApp::from_ffmpeg_lookup(ffmpeg_lookup))
        }),
    )
}
