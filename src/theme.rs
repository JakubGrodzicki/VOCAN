//! Visual theme: the palette and the egui [`Style`] the whole app is drawn with.
//!
//! Everything here is presentation only -- no processing parameter is read or
//! written from this module. It exists so that the colour of a warning label
//! and the colour of the slider it sits under come from one place, instead of
//! being spelled out as a fresh `Color32::from_rgb(..)` at every call site.
//!
//! The palette is a neutral dark graphite with a single amber accent. Amber
//! was not invented for the redesign: it is the colour the app already used
//! for its warnings, so nothing about VOCAN's identity changes here.

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, Margin, Rounding, Stroke,
    TextStyle, Visuals,
};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Window background, behind every panel.
pub const BG: Color32 = Color32::from_rgb(0x0F, 0x11, 0x13);
/// Chrome surfaces: the navigation rail and the action bar.
pub const PANEL: Color32 = Color32::from_rgb(0x12, 0x14, 0x17);
/// A card sitting on [`BG`].
pub const CARD: Color32 = Color32::from_rgb(0x17, 0x1A, 0x1D);
/// A raised control on a card: buttons, dropdowns, the active nav item.
pub const CARD_HI: Color32 = Color32::from_rgb(0x1D, 0x22, 0x26);
/// Recessed control: text fields, slider rails, the log background.
pub const INPUT: Color32 = Color32::from_rgb(0x10, 0x13, 0x15);
/// Hairline border on cards and controls.
pub const LINE: Color32 = Color32::from_rgb(0x23, 0x28, 0x2D);
/// Divider inside a card, one step quieter than [`LINE`].
pub const LINE_SOFT: Color32 = Color32::from_rgb(0x1C, 0x21, 0x26);

/// Primary text.
pub const TXT: Color32 = Color32::from_rgb(0xE7, 0xEA, 0xEC);
/// Labels and secondary text.
pub const TXT2: Color32 = Color32::from_rgb(0x98, 0xA1, 0xA8);
/// Hints, units, inactive metadata.
pub const TXT3: Color32 = Color32::from_rgb(0x63, 0x6C, 0x73);

/// The one accent. Active states, slider fills, the primary button.
pub const AMBER: Color32 = Color32::from_rgb(0xF0, 0xA9, 0x3B);
/// Amber at reading weight, for warning text on a dark background.
pub const AMBER_TEXT: Color32 = Color32::from_rgb(0xE0, 0xAE, 0x55);
/// Amber wash behind a notice.
/// Values are premultiplied: `from_rgba_unmultiplied` is not a `const fn`
/// in ecolor 0.27. These are amber at alpha 22 and 56, red at alpha 18.
pub const AMBER_WASH: Color32 = Color32::from_rgba_premultiplied(21, 15, 5, 22);
/// Amber hairline around a notice.
pub const AMBER_EDGE: Color32 = Color32::from_rgba_premultiplied(53, 37, 13, 56);

/// Errors and the Stop button.
pub const RED: Color32 = Color32::from_rgb(0xE8, 0x61, 0x5F);
/// Error wash behind a log line.
pub const RED_WASH: Color32 = Color32::from_rgba_premultiplied(16, 7, 7, 18);
/// "Ready" states.
pub const GREEN: Color32 = Color32::from_rgb(0x59, 0xC0, 0x8A);

/// Text drawn *on* an amber fill -- the primary button's label.
pub const INK: Color32 = Color32::from_rgb(0x16, 0x19, 0x1C);

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Width of the navigation rail's *content* area.
///
/// This is what `SidePanel::exact_width` takes: the frame's horizontal margins
/// are added outside it, so the rail on screen is this plus 24. Fixed width --
/// it is a list of five known items, so there is nothing for the user to gain
/// by resizing it. Anything that needs the rail's real edge (the divider) must
/// read it from the panel's own rect, not from this constant.
pub const RAIL_W: f32 = 198.0;
/// Height of a standard control (text field, dropdown, small button).
pub const CTRL_H: f32 = 32.0;
/// Height of the primary button.
pub const GO_H: f32 = 44.0;
/// Corner radius on cards.
pub const R_CARD: f32 = 11.0;
/// Corner radius on controls.
pub const R_CTRL: f32 = 8.0;

/// A named font family for the semibold face used by headings.
///
/// Referencing a [`FontFamily::Name`] that was never registered panics on the
/// first render, so [`apply`] falls back to `Proportional` when no semibold
/// face could be found -- which is any platform not covered by `SANS_BOLD`.
const HEADING: &str = "heading";

/// Returns the family to use for headings, given whether the semibold face
/// actually loaded.
pub fn heading_family(loaded: bool) -> FontFamily {
    if loaded {
        FontFamily::Name(HEADING.into())
    } else {
        FontFamily::Proportional
    }
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

/// Loads the fonts and installs the palette on `ctx`.
///
/// Returns `true` if the semibold heading face was found, so the caller can
/// pass it to [`heading_family`].
pub fn apply(ctx: &egui::Context) -> bool {
    let heading_loaded = install_fonts(ctx);
    install_style(ctx, heading_loaded);
    heading_loaded
}

/// Registers a body face, a semibold face for headings and a monospace face
/// for numerics, picking whichever candidate the host actually has.
///
/// Every load is best-effort: a missing file leaves egui's bundled default in
/// place rather than failing to start.
fn install_fonts(ctx: &egui::Context) -> bool {
    let mut fonts = FontDefinitions::default();

    load_first(&mut fonts, "ui_sans", FontFamily::Proportional, SANS);

    // Every number in this app -- LUFS, dB, percentages, file counts, paths --
    // is drawn in the monospace family, so that digits line up in a column and
    // a value stops shifting sideways while a slider is dragged.
    load_first(&mut fonts, "ui_mono", FontFamily::Monospace, MONO);

    // The only family whose absence is actually visible: with no semibold face
    // the section headings fall back to the body weight and stop reading as
    // headings at all. `install_style` handles that through `heading_family`.
    let heading_loaded = load_first(
        &mut fonts,
        "ui_sans_bold",
        FontFamily::Name(HEADING.into()),
        SANS_BOLD,
    );

    ctx.set_fonts(fonts);
    heading_loaded
}

/// Registers the first readable file in `candidates` at the front of `family`.
///
/// Best-effort by design: when nothing matches, egui's own bundled faces
/// (Ubuntu-Light and Hack, both compiled into the binary) stay in place, so a
/// platform we have no path for still renders every glyph. Inserting at the
/// front rather than replacing keeps those bundled faces as the fallback for
/// characters our pick happens to lack.
fn load_first(
    fonts: &mut FontDefinitions,
    name: &str,
    family: FontFamily,
    candidates: &[&str],
) -> bool {
    for path in candidates {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        // `.ttc` collections parse as long as a face index is given, and face
        // 0 is the regular one in every collection listed below.
        fonts
            .font_data
            .insert(name.to_owned(), FontData::from_owned(data));
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, name.to_owned());
        return true;
    }
    false
}

// Font candidates, most-preferred first, Windows then macOS then Linux. Only
// paths that exist on a stock install are listed -- this runs on every start,
// and a miss costs one failed `read`.
//
// macOS keeps Helvetica and Menlo in `.ttc` collections; SF Pro is variable-
// weight, so there is no static semibold file to point at, and macOS falls
// back to Arial Bold for headings.

/// Body and UI text.
const SANS: &[&str] = &[
    "C:/Windows/Fonts/segoeui.ttf",
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/SFNSText.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

/// Section headings.
const SANS_BOLD: &[&str] = &[
    "C:/Windows/Fonts/seguisb.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "/Library/Fonts/Arial Bold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
];

/// Numeric read-outs and paths.
const MONO: &[&str] = &[
    "C:/Windows/Fonts/CascadiaMono.ttf",
    "C:/Windows/Fonts/CascadiaCode.ttf",
    "C:/Windows/Fonts/consola.ttf",
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
];

/// Installs the text scale, spacing and [`Visuals`].
fn install_style(ctx: &egui::Context, heading_loaded: bool) {
    let mut style = (*ctx.style()).clone();

    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(12.5, FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(13.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(19.0, heading_family(heading_loaded)),
        ),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(9.0, 8.0);
    style.spacing.button_padding = egui::vec2(11.0, 7.0);
    style.spacing.interact_size = egui::vec2(40.0, CTRL_H);
    style.spacing.slider_rail_height = 5.0;
    style.spacing.icon_width = 17.0;
    style.spacing.icon_width_inner = 10.0;
    style.spacing.menu_margin = Margin::same(6.0);
    style.spacing.combo_height = 320.0;

    let mut v = Visuals::dark();

    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_rounding = Rounding::same(R_CARD);
    v.menu_rounding = Rounding::same(R_CTRL);
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = CARD_HI;
    v.hyperlink_color = AMBER;
    v.warn_fg_color = AMBER_TEXT;
    v.error_fg_color = RED;

    // Sliders take their trailing fill and progress bars their fill from
    // `selection.bg_fill`, so this one line is what makes both amber.
    v.selection.bg_fill = AMBER;
    v.selection.stroke = Stroke::new(1.0, INK);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;

    // A group/frame drawn by egui itself.
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.weak_bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE_SOFT);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TXT2);
    v.widgets.noninteractive.rounding = Rounding::same(R_CARD);

    // Resting button / dropdown / slider rail.
    v.widgets.inactive.bg_fill = INPUT;
    v.widgets.inactive.weak_bg_fill = CARD_HI;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TXT);
    v.widgets.inactive.rounding = Rounding::same(R_CTRL);
    v.widgets.inactive.expansion = 0.0;

    v.widgets.hovered.bg_fill = CARD_HI;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x24, 0x2A, 0x2F);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x39, 0x41, 0x48));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TXT);
    v.widgets.hovered.rounding = Rounding::same(R_CTRL);
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.bg_fill = Color32::from_rgb(0x2A, 0x31, 0x38);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x2A, 0x31, 0x38);
    v.widgets.active.bg_stroke = Stroke::new(1.0, AMBER);
    v.widgets.active.fg_stroke = Stroke::new(1.0, TXT);
    v.widgets.active.rounding = Rounding::same(R_CTRL);
    v.widgets.active.expansion = 0.0;

    // An open combo box keeps the amber edge so the popup reads as attached.
    v.widgets.open.bg_fill = CARD_HI;
    v.widgets.open.weak_bg_fill = CARD_HI;
    v.widgets.open.bg_stroke = Stroke::new(1.0, AMBER_EDGE);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TXT);
    v.widgets.open.rounding = Rounding::same(R_CTRL);

    style.visuals = v;
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_first_reports_failure_and_changes_nothing_when_no_candidate_exists() {
        let mut fonts = FontDefinitions::default();
        let before = fonts.families.get(&FontFamily::Proportional).cloned();

        let loaded = load_first(
            &mut fonts,
            "nope",
            FontFamily::Proportional,
            &["/definitely/not/a/real/font.ttf"],
        );

        assert!(!loaded);
        assert!(!fonts.font_data.contains_key("nope"));
        assert_eq!(
            fonts.families.get(&FontFamily::Proportional).cloned(),
            before,
            "a failed load must leave egui's bundled fallback chain untouched"
        );
    }

    #[test]
    fn heading_family_falls_back_to_proportional_when_no_bold_face_loaded() {
        // Referencing a `FontFamily::Name` that was never registered panics on
        // the first render. On any host without one of the `SANS_BOLD`
        // candidates this fallback is the only thing keeping the app starting
        // at all, so it is worth a test of its own.
        assert_eq!(heading_family(false), FontFamily::Proportional);
        assert_eq!(heading_family(true), FontFamily::Name(HEADING.into()));
    }

    #[test]
    fn every_font_candidate_is_an_absolute_path() {
        // A relative path would resolve against the working directory, which
        // for a GUI launched from a shortcut is wherever the shell happened to
        // be -- so it would load a font, or not, depending on how it was
        // started.
        for (name, list) in [("SANS", SANS), ("SANS_BOLD", SANS_BOLD), ("MONO", MONO)] {
            assert!(!list.is_empty(), "{name} lists no candidates");
            for path in list {
                assert!(
                    path.starts_with('/') || path.contains(":/"),
                    "{name} candidate {path:?} is not absolute"
                );
            }
        }
    }
}
