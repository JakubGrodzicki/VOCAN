#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
use vocan::{app, ffmpeg};

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
    let heading_font_loaded = if let Ok(data) = std::fs::read("C:/Windows/Fonts/seguisb.ttf") {
        fonts
            .font_data
            .insert("segoe_ui_sb".to_owned(), egui::FontData::from_owned(data));
        fonts
            .families
            .entry(egui::FontFamily::Name("heading".into()))
            .or_default()
            .insert(0, "segoe_ui_sb".to_owned());
        true
    } else {
        false
    };

    ctx.set_fonts(fonts);

    // Slightly larger text sizes for comfortable reading on high-DPI displays.
    let mut style = (*ctx.style()).clone();
    // Only use the "heading" font family if we actually registered a font
    // under that name above -- referencing an unregistered FontFamily::Name
    // panics on first render. This is the case on any non-Windows platform,
    // where the Segoe UI Semibold file simply doesn't exist.
    let heading_family = if heading_font_loaded {
        egui::FontFamily::Name("heading".into())
    } else {
        egui::FontFamily::Proportional
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
            .with_inner_size([750.0, 900.0])
            .with_min_inner_size([600.0, 550.0]),
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
            Box::new(app::AudioBatchApp::from_ffmpeg_lookup(ffmpeg_lookup))
        }),
    )
}
