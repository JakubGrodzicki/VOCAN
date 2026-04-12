#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod audio_effects;
mod ffmpeg;
mod processing;
mod types;

use eframe::egui;
use std::path::PathBuf;

fn configure_fonts_and_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Load Segoe UI from Windows system fonts (present on all Windows 10/11).
    if let Ok(data) = std::fs::read("C:/Windows/Fonts/segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe_ui".to_owned(), egui::FontData::from_owned(data));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe_ui".to_owned());
    }

    // Load Segoe UI Semibold for headings.
    if let Ok(data) = std::fs::read("C:/Windows/Fonts/seguisb.ttf") {
        fonts
            .font_data
            .insert("segoe_ui_sb".to_owned(), egui::FontData::from_owned(data));
        fonts
            .families
            .entry(egui::FontFamily::Name("heading".into()))
            .or_default()
            .insert(0, "segoe_ui_sb".to_owned());
    }

    ctx.set_fonts(fonts);

    // Slightly larger text sizes for comfortable reading on high-DPI displays.
    let mut style = (*ctx.style()).clone();
    let heading_family = if style.text_styles.is_empty() {
        egui::FontFamily::Proportional
    } else {
        egui::FontFamily::Name("heading".into())
    };

    style.text_styles = [
        (
            egui::TextStyle::Small,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Body,
            egui::FontId::new(15.0, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Monospace,
            egui::FontId::new(14.0, egui::FontFamily::Monospace),
        ),
        (
            egui::TextStyle::Button,
            egui::FontId::new(15.0, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Heading,
            egui::FontId::new(22.0, heading_family),
        ),
    ]
    .into();

    // More generous padding so controls are easier to hit on 4K touch/trackpad.
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);

    ctx.set_style(style);
}

fn main() -> eframe::Result<()> {
    let ffmpeg_path = ffmpeg::find_ffmpeg().unwrap_or_else(|e| {
        eprintln!("{}", e);
        PathBuf::from("ffmpeg")
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([750.0, 750.0]),
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
            configure_fonts_and_style(&cc.egui_ctx);
            Box::new(app::AudioBatchApp::new(ffmpeg_path))
        }),
    )
}
