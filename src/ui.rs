//! Custom widgets.
//!
//! egui's stock checkbox and button draw the same shape whether they are on or
//! off -- only a checkmark glyph or the text colour changes -- which is too
//! quiet for controls that decide what happens to a batch of audio. The
//! widgets here paint an explicit on-state in the accent colour.
//!
//! Everything in this module is drawing plus click handling. No widget here
//! knows what the value it toggles means.

use std::ops::RangeInclusive;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Rect, Response, Rounding, Sense, Stroke, TextStyle, Ui,
    Vec2,
};

use crate::theme;

// ---------------------------------------------------------------------------
// Toggles
// ---------------------------------------------------------------------------

/// A pill switch, for the master on/off of a whole section.
///
/// Animated through [`egui::Context::animate_bool_with_time`], so the knob
/// slides rather than jumping -- the one piece of motion in the app, and it
/// only ever runs on a control the user just clicked.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(38.0, 21.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_with_time(response.id, *on, 0.12);
        let enabled = ui.is_enabled();

        let track = if *on {
            theme::AMBER
        } else {
            Color32::from_rgb(0x2A, 0x30, 0x36)
        };
        let track = if enabled {
            track
        } else {
            track.gamma_multiply(0.5)
        };
        ui.painter()
            .rect_filled(rect, Rounding::same(rect.height() / 2.0), track);

        let pad = 3.0;
        let r = (rect.height() - pad * 2.0) / 2.0;
        let x = egui::lerp((rect.left() + pad + r)..=(rect.right() - pad - r), how_on);
        let knob = if *on {
            theme::INK
        } else {
            Color32::from_rgb(0x6A, 0x73, 0x7A)
        };
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), r, knob);
    }
    response
}

/// A square checkbox that fills with the accent colour when checked.
///
/// Takes the label as well as the value so that clicking the text toggles it
/// too -- the hit target is the whole row, not the 17px box.
pub fn check(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    check_colored(ui, on, label, theme::TXT)
}

/// [`check`] with an explicit label colour, for sub-options that should read
/// one step quieter than the module they belong to.
pub fn check_colored(ui: &mut Ui, on: &mut bool, label: &str, text: Color32) -> Response {
    let box_size = 17.0;
    let gap = 9.0;
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), TextStyle::Body.resolve(ui.style()), text);
    let size = Vec2::new(
        box_size + gap + galley.size().x,
        galley.size().y.max(box_size).max(20.0),
    );
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };

        let bx = Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - box_size / 2.0),
            Vec2::splat(box_size),
        );
        if *on {
            ui.painter()
                .rect_filled(bx, Rounding::same(5.0), dim(theme::AMBER));
            // Checkmark, drawn rather than typed: the bundled icon fonts have
            // no glyph that sits right in a 17px box at this weight.
            let c = bx.center();
            let s = box_size;
            ui.painter().add(egui::Shape::line(
                vec![
                    egui::pos2(c.x - s * 0.22, c.y + s * 0.02),
                    egui::pos2(c.x - s * 0.06, c.y + s * 0.18),
                    egui::pos2(c.x + s * 0.24, c.y - s * 0.20),
                ],
                Stroke::new(1.9, dim(theme::INK)),
            ));
        } else {
            let hovered = response.hovered() && enabled;
            ui.painter()
                .rect_filled(bx, Rounding::same(5.0), dim(theme::INPUT));
            ui.painter().rect_stroke(
                bx,
                Rounding::same(5.0),
                Stroke::new(
                    1.0,
                    dim(if hovered {
                        Color32::from_rgb(0x4A, 0x53, 0x5A)
                    } else {
                        Color32::from_rgb(0x33, 0x3A, 0x41)
                    }),
                ),
            );
        }

        let text_pos = egui::pos2(bx.right() + gap, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(
            text_pos,
            ui.painter().layout_no_wrap(
                label.to_owned(),
                TextStyle::Body.resolve(ui.style()),
                dim(text),
            ),
            dim(text),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// Segmented control
// ---------------------------------------------------------------------------

/// A one-of-N picker drawn as a row of attached chips.
///
/// This exists for the noise-reduction pair. Spectral Gate and nnnoiseless
/// occupy the same slot in the chain and were two checkboxes that greyed each
/// other out -- which looks like two independent options that happen to be
/// broken. As a segmented control the exclusivity is the shape of the widget,
/// and "neither" becomes a first-class choice instead of an absence.
///
/// Returns `Some(index)` on the frame the selection changes.
pub fn segmented(ui: &mut Ui, selected: usize, options: &[&str]) -> Option<usize> {
    let h = 34.0;
    let pad = 3.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());

    ui.painter()
        .rect_filled(rect, Rounding::same(9.0), theme::INPUT);
    ui.painter()
        .rect_stroke(rect, Rounding::same(9.0), Stroke::new(1.0, theme::LINE));

    let enabled = ui.is_enabled();
    let inner = rect.shrink(pad);
    let seg_w = inner.width() / options.len() as f32;
    let mut changed = None;

    for (i, label) in options.iter().enumerate() {
        let seg = Rect::from_min_size(
            egui::pos2(inner.left() + seg_w * i as f32, inner.top()),
            Vec2::new(seg_w, inner.height()),
        );
        let id = ui.id().with(("seg", i));
        let response = ui.interact(seg, id, Sense::click());
        if response.clicked() && enabled {
            changed = Some(i);
        }

        let active = i == selected;
        if active {
            ui.painter().rect_filled(
                seg.shrink2(Vec2::new(1.0, 0.0)),
                Rounding::same(6.0),
                Color32::from_rgb(0x26, 0x2C, 0x32),
            );
        } else if response.hovered() && enabled {
            ui.painter().rect_filled(
                seg.shrink2(Vec2::new(1.0, 0.0)),
                Rounding::same(6.0),
                Color32::from_rgb(0x1A, 0x1E, 0x22),
            );
        }

        let color = match (active, enabled) {
            (true, true) => theme::AMBER,
            (false, true) => theme::TXT2,
            (true, false) => theme::AMBER.gamma_multiply(0.45),
            (false, false) => theme::TXT2.gamma_multiply(0.45),
        };
        ui.painter().text(
            seg.center(),
            Align2::CENTER_CENTER,
            label,
            TextStyle::Body.resolve(ui.style()),
            color,
        );
    }
    changed
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// The card every group of controls sits in.
pub fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::none()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::LINE))
        .rounding(Rounding::same(theme::R_CARD))
        .inner_margin(egui::Margin::symmetric(15.0, 14.0))
        .show(ui, |ui| {
            // A `Frame` shrink-wraps its content, so a card holding only a
            // checkbox and a line of hint text came out visibly narrower than
            // the card below it. Cards in a column should share one edge.
            ui.set_min_width(ui.available_width());
            add(ui);
        });
}

/// A value read-out that can also be typed into: monospace, boxed.
///
/// Visually the box a slider's number sits in; functionally a text field, so a
/// value that is awkward to land on by dragging -- -14.7 LUFS on a 17 dB
/// track -- can simply be entered. `decimals` and `suffix` are how the number
/// is rendered when the field is idle, so the caller no longer formats the
/// read-out itself.
///
/// `min_width` keeps a column of fields the same width as their values change
/// ("80%" next to "100%"); a value too wide for it grows the field rather than
/// spilling out of it, which is what a font with wider digits would do.
pub fn value_field(
    ui: &mut Ui,
    id_source: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    decimals: usize,
    suffix: &str,
    min_width: f32,
) -> Response {
    let id = ui.make_persistent_id(id_source);
    let font = TextStyle::Monospace.resolve(ui.style());

    // The edit buffer exists only while the field has focus. Re-deriving the
    // text from `value` on a focused field would fight every keystroke -- "-1"
    // rewritten to "-1.0" before the "4" of -14 is typed -- and on an unfocused
    // one `value` is the only truth, so a slider drag needs nothing kept in
    // step here.
    let mut text = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| format!("{value:.decimals$}{suffix}"));

    let text_w = ui
        .painter()
        .layout_no_wrap(text.clone(), font.clone(), theme::TXT)
        .size()
        .x;
    let width = min_width.max(text_w + 16.0);
    let row_h = ui.fonts(|f| f.row_height(&font));
    let pad_y = ((22.0 - row_h) / 2.0).max(0.0);

    // Reserved up front so the box paints *behind* the text. Its outline says
    // whether the field has focus, and that is only known once the TextEdit
    // has run -- hence a placeholder now and the real shape after.
    let box_shape = ui.painter().add(egui::Shape::Noop);

    let enabled = ui.is_enabled();
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id(id)
            .font(TextStyle::Monospace)
            .frame(false)
            .margin(Vec2::new(4.0, pad_y))
            .desired_width(width - 8.0)
            .horizontal_align(Align::Center)
            .vertical_align(Align::Center)
            // Long enough for "-100.0", short enough that the field cannot
            // grow far past the column the sliders were laid out around.
            .char_limit(7)
            .text_color(if enabled {
                theme::TXT
            } else {
                theme::TXT.gamma_multiply(0.45)
            }),
    );

    let focused = response.has_focus();
    ui.painter().set(
        box_shape,
        egui::Shape::Vec(vec![
            egui::Shape::rect_filled(response.rect, Rounding::same(6.0), theme::INPUT),
            egui::Shape::rect_stroke(
                response.rect,
                Rounding::same(6.0),
                Stroke::new(
                    1.0,
                    if focused {
                        theme::AMBER
                    } else {
                        Color32::from_rgb(0x27, 0x2D, 0x33)
                    },
                ),
            ),
        ]),
    );

    // Applied on every keystroke that parses, so the slider tracks the field
    // as it is typed. Text that is not a number *yet* -- an empty field, a
    // lone "-" -- is left alone to become one, rather than snapping the value
    // to zero and dragging the slider to its floor mid-edit.
    if response.changed() {
        if let Some(parsed) = parse_number(&text) {
            *value = parsed.clamp(*range.start(), *range.end());
        }
    }

    if focused {
        ui.data_mut(|d| d.insert_temp(id, text));
    } else {
        // Dropping the buffer is what re-formats the field: with nothing
        // stored, next frame's text comes from `value` again -- rounded,
        // clamped and with its unit back on.
        ui.data_mut(|d| d.remove::<String>(id));
    }

    response
}

/// Reads a number out of whatever is in a [`value_field`], ignoring the unit.
///
/// The field shows its unit ("50%"), and select-all-then-retype is not the
/// only way people edit: typing "5" in front of an existing "0%" has to keep
/// working. Everything that is not part of a number is dropped, which also
/// makes a pasted "-14.7 LUFS" land as -14.7.
fn parse_number(text: &str) -> Option<f32> {
    let digits: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    digits.parse().ok()
}

/// A small all-caps tag, for metadata like "compute heavy".
pub fn tag(ui: &mut Ui, text: &str) {
    let font = FontId::new(10.5, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), theme::TXT2);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 14.0, 18.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, Rounding::same(5.0), theme::INPUT);
    ui.painter().rect_stroke(
        rect,
        Rounding::same(5.0),
        Stroke::new(1.0, Color32::from_rgb(0x27, 0x2D, 0x33)),
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        font,
        theme::TXT2,
    );
}

/// A tinted, bordered block of warning text with a triangle glyph.
///
/// Replaces the bare `colored_label` in amber that these warnings used to be:
/// same words, but bounded, so a four-line caveat no longer reads as louder
/// than the controls above it.
pub fn notice(ui: &mut Ui, text: &str) {
    egui::Frame::none()
        .fill(theme::AMBER_WASH)
        .stroke(Stroke::new(1.0, theme::AMBER_EDGE))
        .rounding(Rounding::same(theme::R_CTRL))
        .inner_margin(egui::Margin::symmetric(11.0, 9.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let (tri, _) = ui.allocate_exact_size(Vec2::new(14.0, 16.0), Sense::hover());
                warning_triangle(ui, tri, theme::AMBER_TEXT);
                ui.add_space(1.0);
                // Must be an explicit wrapping `Label`: a plain `ui.label`
                // inside a horizontal layout does not wrap, so the long
                // automixer caveat ran straight off the right of the window.
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(text)
                            .size(11.5)
                            .color(theme::AMBER_TEXT),
                    )
                    .wrap(true),
                );
            });
        });
}

/// Draws the warning triangle used by [`notice`] and the FFmpeg banner.
pub fn warning_triangle(ui: &Ui, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = 6.4;
    let stroke = Stroke::new(1.2, color);
    ui.painter().add(egui::Shape::closed_line(
        vec![
            egui::pos2(c.x, c.y - s),
            egui::pos2(c.x + s * 1.05, c.y + s * 0.8),
            egui::pos2(c.x - s * 1.05, c.y + s * 0.8),
        ],
        stroke,
    ));
    ui.painter().line_segment(
        [
            egui::pos2(c.x, c.y - s * 0.30),
            egui::pos2(c.x, c.y + s * 0.20),
        ],
        Stroke::new(1.3, color),
    );
    ui.painter()
        .circle_filled(egui::pos2(c.x, c.y + s * 0.52), 0.9, color);
}

/// A quiet one-line hint under a control.
pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(11.0).color(theme::TXT3));
}

/// A full-width horizontal divider inside a card.
pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, Rounding::ZERO, theme::LINE_SOFT);
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// The primary action: a full-width amber bar with a play triangle.
///
/// Drawn rather than assembled from `egui::Button` because the disabled state
/// has to stay legible -- a greyed-out stock button was the single biggest
/// reason the old window read as having no primary action at all.
pub fn go_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), theme::GO_H),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );

    let (fill, text) = if !enabled {
        (Color32::from_rgb(0x1E, 0x22, 0x26), theme::TXT3)
    } else if response.is_pointer_button_down_on() {
        (Color32::from_rgb(0xD9, 0x94, 0x2A), theme::INK)
    } else if response.hovered() {
        (Color32::from_rgb(0xFF, 0xBC, 0x55), theme::INK)
    } else {
        (theme::AMBER, theme::INK)
    };

    ui.painter().rect_filled(rect, Rounding::same(10.0), fill);
    if !enabled {
        ui.painter().rect_stroke(
            rect,
            Rounding::same(10.0),
            Stroke::new(1.0, Color32::from_rgb(0x2A, 0x30, 0x36)),
        );
    }

    let font = FontId::new(14.5, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), text);
    let tri_w = 13.0;
    let gap = 10.0;
    let total = tri_w + gap + galley.size().x;
    let left = rect.center().x - total / 2.0;

    let c = egui::pos2(left + tri_w / 2.0, rect.center().y);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x - 4.5, c.y - 6.5),
            egui::pos2(c.x + 6.0, c.y),
            egui::pos2(c.x - 4.5, c.y + 6.5),
        ],
        text,
        Stroke::NONE,
    ));
    ui.painter().text(
        egui::pos2(left + tri_w + gap, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        text,
    );

    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// The Stop button shown while a batch runs: red, outlined, never filled, so
/// it cannot be mistaken for the primary action.
pub fn stop_button(ui: &mut Ui) -> Response {
    let font = FontId::new(13.0, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap("Stop".to_owned(), font.clone(), theme::RED);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 40.0, 34.0), Sense::click());

    let wash = if response.hovered() {
        Color32::from_rgba_unmultiplied(0xE8, 0x61, 0x5F, 0x2A)
    } else {
        Color32::from_rgba_unmultiplied(0xE8, 0x61, 0x5F, 0x1A)
    };
    ui.painter()
        .rect_filled(rect, Rounding::same(theme::R_CTRL), wash);
    ui.painter().rect_stroke(
        rect,
        Rounding::same(theme::R_CTRL),
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xE8, 0x61, 0x5F, 0x60)),
    );

    let sq = Rect::from_center_size(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        Vec2::splat(9.0),
    );
    ui.painter()
        .rect_filled(sq, Rounding::same(1.6), theme::RED);
    ui.painter().text(
        egui::pos2(sq.right() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        "Stop",
        font,
        theme::RED,
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small secondary button: outlined, quiet, for "Browse", "Analyze folder",
/// "Set as target".
pub fn small_button(ui: &mut Ui, label: &str) -> Response {
    let font = TextStyle::Body.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), theme::TXT);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 24.0, 30.0), Sense::click());

    let enabled = ui.is_enabled();
    let fill = if response.hovered() && enabled {
        Color32::from_rgb(0x24, 0x2A, 0x2F)
    } else {
        theme::CARD_HI
    };
    let text = if enabled {
        theme::TXT
    } else {
        theme::TXT.gamma_multiply(0.45)
    };
    ui.painter()
        .rect_filled(rect, Rounding::same(theme::R_CTRL), fill);
    ui.painter().rect_stroke(
        rect,
        Rounding::same(theme::R_CTRL),
        Stroke::new(
            1.0,
            if enabled {
                Color32::from_rgb(0x2C, 0x32, 0x38)
            } else {
                theme::LINE_SOFT
            },
        ),
    );
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, label, font, text);

    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

// ---------------------------------------------------------------------------
// Navigation rail
// ---------------------------------------------------------------------------

/// The glyph drawn at the left of a nav item.
///
/// Line art rather than font glyphs: the bundled icon fonts cover none of
/// these shapes, and five polylines cost less than shipping an icon font.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Folder,
    Wave,
    Bars,
    List,
}

fn draw_icon(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let p = ui.painter();
    let s = Stroke::new(1.2, color);
    let c = rect.center();
    let u = rect.width() / 16.0; // the icons are drawn on a 16x16 grid
    let at = |x: f32, y: f32| egui::pos2(c.x + (x - 8.0) * u, c.y + (y - 8.0) * u);

    match icon {
        Icon::Folder => {
            p.add(egui::Shape::closed_line(
                vec![
                    at(1.5, 12.9),
                    at(1.5, 4.2),
                    at(5.7, 4.2),
                    at(7.2, 5.6),
                    at(14.1, 5.6),
                    at(14.1, 12.9),
                ],
                s,
            ));
        }
        Icon::Wave => {
            p.add(egui::Shape::line(
                vec![
                    at(2.0, 8.0),
                    at(4.2, 8.0),
                    at(5.8, 3.6),
                    at(8.0, 12.4),
                    at(10.2, 6.2),
                    at(11.5, 8.0),
                    at(14.0, 8.0),
                ],
                s,
            ));
        }
        Icon::Bars => {
            for (x, top, bot) in [
                (3.0, 6.0, 11.0),
                (6.3, 3.5, 12.5),
                (9.7, 5.5, 10.5),
                (13.0, 4.0, 12.0),
            ] {
                p.line_segment([at(x, top), at(x, bot)], s);
            }
        }
        Icon::List => {
            for (y, x2) in [(3.6, 12.6), (6.8, 12.6), (10.0, 9.6), (13.2, 7.6)] {
                p.line_segment([at(3.4, y), at(x2, y)], s);
            }
        }
    }
}

/// One row in the navigation rail.
///
/// `summary` is the point of the whole design: with only one section on screen
/// at a time, the rail is where the rest of the settings stay visible. Each
/// row carries the live value of the section it points at, so nothing is
/// hidden behind a click.
pub fn nav_item(
    ui: &mut Ui,
    active: bool,
    icon: Icon,
    label: &str,
    summary: &str,
    summary_color: Color32,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());

    if active {
        ui.painter()
            .rect_filled(rect, Rounding::same(9.0), theme::CARD_HI);
        // The accent bar is what marks the current section. It sits inside the
        // rounded rect rather than outside it so the rail keeps a clean edge.
        let bar = Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 9.0),
            Vec2::new(2.5, rect.height() - 18.0),
        );
        ui.painter()
            .rect_filled(bar, Rounding::same(1.5), theme::AMBER);
    } else if response.hovered() {
        ui.painter().rect_filled(
            rect,
            Rounding::same(9.0),
            Color32::from_rgb(0x18, 0x1C, 0x1F),
        );
    }

    let icon_rect = Rect::from_center_size(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        Vec2::splat(15.0),
    );
    draw_icon(
        ui,
        icon_rect,
        icon,
        if active { theme::AMBER } else { theme::TXT2 },
    );

    let label_font = TextStyle::Body.resolve(ui.style());
    let label_left = icon_rect.right() + 10.0;
    ui.painter().text(
        egui::pos2(label_left, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        label_font.clone(),
        if active { theme::TXT } else { theme::TXT2 },
    );

    // The summary is right-aligned in a 198px rail, so it gets only what the
    // label leaves behind. Clipped to exactly that gap: an over-long summary
    // used to run under the label and out through the rail's edge. Clipping
    // rather than hiding keeps "not set" -- the one summary that means the
    // run cannot start -- on screen no matter how narrow the gap gets.
    if !summary.is_empty() {
        let label_w = ui
            .painter()
            .layout_no_wrap(label.to_owned(), label_font, theme::TXT2)
            .size()
            .x;
        let gap = Rect::from_min_max(
            egui::pos2(label_left + label_w + 8.0, rect.top()),
            egui::pos2(rect.right() - 11.0, rect.bottom()),
        );
        if gap.width() > 0.0 {
            ui.painter().with_clip_rect(gap).text(
                gap.right_center(),
                Align2::RIGHT_CENTER,
                summary,
                FontId::new(10.5, egui::FontFamily::Monospace),
                summary_color,
            );
        }
    }

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The wordmark at the top of the rail: an amber tile with three bars, then
/// "VOCAN".
pub fn brand(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let tile = Rect::from_min_size(
        egui::pos2(rect.left() + 4.0, rect.center().y - 11.0),
        Vec2::splat(22.0),
    );
    ui.painter()
        .rect_filled(tile, Rounding::same(6.0), theme::AMBER);
    let u = tile.width() / 20.0;
    let o = tile.min;
    for (x, y, h) in [(5.0, 8.0, 4.0), (8.4, 5.4, 9.2), (11.8, 7.0, 6.0)] {
        ui.painter().rect_filled(
            Rect::from_min_size(
                egui::pos2(o.x + x * u, o.y + y * u),
                Vec2::new(1.8 * u, h * u),
            ),
            Rounding::same(0.9 * u),
            theme::INK,
        );
    }
    ui.painter().text(
        egui::pos2(tile.right() + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        "VOCAN",
        FontId::new(16.0, egui::FontFamily::Proportional),
        theme::TXT,
    );
}

/// The small all-caps group heading in the rail ("CHAIN", "RUN").
pub fn rail_group(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::new(10.0, egui::FontFamily::Proportional),
        Color32::from_rgb(0x56, 0x5F, 0x66),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_number_reads_a_value_through_its_unit() {
        // What a `value_field` actually contains: the number plus whatever
        // suffix the caller renders it with, and a pasted measurement from the
        // loudness readout above it.
        assert_eq!(parse_number("50%"), Some(50.0));
        assert_eq!(parse_number("-3.0"), Some(-3.0));
        assert_eq!(parse_number("-14.7 LUFS"), Some(-14.7));
        assert_eq!(parse_number("  100  "), Some(100.0));
    }

    #[test]
    fn parse_number_rejects_text_that_is_not_a_number_yet() {
        // These are the states a field passes through mid-edit. Returning
        // `Some(0.0)` for any of them would drag the slider to its floor
        // between two keystrokes, which is why the caller needs `None`.
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("-"), None);
        assert_eq!(parse_number("%"), None);
        assert_eq!(parse_number("."), None);
        assert_eq!(parse_number("abc"), None);
    }
}
