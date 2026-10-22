//! The app's look, shared by the launcher, its dialogs and the session
//! menu: palette, egui style and a few building blocks.

use egui::{Color32, CornerRadius, RichText, Stroke, vec2};

// Palette, matching the icon.
pub const BG: Color32 = Color32::from_rgb(0x17, 0x1a, 0x26);
pub const CARD: Color32 = Color32::from_rgb(0x22, 0x27, 0x38);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x2c, 0x33, 0x4a);
pub const FRAME: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x45);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xc4, 0x4d);
pub const BLUE: Color32 = Color32::from_rgb(0x6c, 0x9c, 0xf0);
pub const DEEP_BLUE: Color32 = Color32::from_rgb(0x3d, 0x5a, 0x99);
pub const MUTED: Color32 = Color32::from_rgb(0x9a, 0xa3, 0xb8);
pub const GREEN: Color32 = Color32::from_rgb(0x5f, 0xd0, 0x7f);
pub const RED: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x75);

/// Dark, rounded, a little roomier than egui's default.
pub fn style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.window_stroke = Stroke::new(1.0, CARD_HOVER);
    visuals.extreme_bg_color = Color32::from_rgb(0x12, 0x14, 0x1d);
    visuals.selection.bg_fill = DEEP_BLUE;
    visuals.hyperlink_color = BLUE;
    visuals.window_corner_radius = CornerRadius::same(14);
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(8);
    }
    visuals.widgets.inactive.weak_bg_fill = CARD_HOVER;
    visuals.widgets.inactive.bg_fill = CARD_HOVER;
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x38, 0x41, 0x5e);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x38, 0x41, 0x5e);
    visuals.widgets.active.weak_bg_fill = DEEP_BLUE;
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(10.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 28.0;
        for (text_style, size) in [
            (egui::TextStyle::Body, 15.0),
            (egui::TextStyle::Button, 15.0),
            (egui::TextStyle::Heading, 22.0),
            (egui::TextStyle::Small, 12.0),
        ] {
            if let Some(f) = s.text_styles.get_mut(&text_style) {
                f.size = size;
            }
        }
    });
}

/// The frame of every dialog and of the session menu.
pub fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(14)
        .inner_margin(20)
        .stroke(Stroke::new(1.0, CARD_HOVER))
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(110),
        })
}

pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(Color32::BLACK)).fill(AMBER)
}

pub fn danger_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(Color32::WHITE))
        .fill(Color32::from_rgb(0x8c, 0x3a, 0x45))
}

/// A small uppercase heading for a group of rows.
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(12.0)
            .strong()
            .color(MUTED),
    );
    ui.add_space(2.0);
}

/// Secondary text: hints and notes.
pub fn note(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(MUTED)).wrap());
}

/// A dialog's title row: heading, optional subtitle, and a close button.
/// Returns whether the close button was clicked.
pub fn title_bar(ui: &mut egui::Ui, title: &str, subtitle: Option<&str>) -> bool {
    let mut close = false;
    let width = ui.available_width();
    ui.horizontal(|ui| {
        // Leave room for the close button; long text wraps.
        ui.vertical(|ui| {
            ui.set_max_width(width - 40.0);
            ui.add(egui::Label::new(RichText::new(title).heading().color(Color32::WHITE)).wrap());
            if let Some(sub) = subtitle {
                ui.add(egui::Label::new(RichText::new(sub).size(12.5).color(MUTED)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            close = close_button(ui).clicked();
        });
    });
    close
}

/// A round ✕ button, drawn (not a glyph, which fonts may lack).
pub fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(28.0, 28.0), egui::Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.circle_filled(rect.center(), 14.0, CARD_HOVER);
    }
    let color = if response.hovered() {
        Color32::WHITE
    } else {
        MUTED
    };
    let r = 5.0;
    let c = rect.center();
    let stroke = Stroke::new(1.8, color);
    painter.line_segment([c + vec2(-r, -r), c + vec2(r, r)], stroke);
    painter.line_segment([c + vec2(-r, r), c + vec2(r, -r)], stroke);
    response.on_hover_text("Close")
}

/// A "⋯" button drawn over `rect` without taking part in layout (e.g. in
/// a card's corner).
pub fn more_button(ui: &mut egui::Ui, rect: egui::Rect, id: egui::Id) -> egui::Response {
    let response = ui.interact(rect, id, egui::Sense::click());
    let painter = ui.painter();
    if response.hovered() || response.is_pointer_button_down_on() {
        painter.circle_filled(rect.center(), 14.0, CARD_HOVER);
    }
    let color = if response.hovered() {
        Color32::WHITE
    } else {
        MUTED
    };
    for dx in [-6.0, 0.0, 6.0] {
        painter.circle_filled(rect.center() + vec2(dx, 0.0), 2.0, color);
    }
    response
}

/// Tabs as a row of pills; returns whether the tab changed.
pub fn tab_bar<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    current: &mut T,
    tabs: &[(T, &str)],
) -> bool {
    let mut changed = false;
    // Wraps onto more lines in narrow windows.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for &(tab, label) in tabs {
            if ui.selectable_label(*current == tab, label).clicked() && *current != tab {
                *current = tab;
                changed = true;
            }
        }
    });
    changed
}

/// The app logo: a frame with one lit pane.
pub fn paint_logo(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 6, FRAME);
    let inner = rect.shrink(3.0);
    let half = inner.width() / 2.0 - 1.0;
    let panes = [
        (inner.left_top(), AMBER),
        (inner.left_top() + vec2(half + 2.0, 0.0), BLUE),
        (inner.left_top() + vec2(0.0, half + 2.0), BLUE),
        (inner.left_top() + vec2(half + 2.0, half + 2.0), BLUE),
    ];
    for (pos, color) in panes {
        painter.rect_filled(egui::Rect::from_min_size(pos, vec2(half, half)), 2, color);
    }
}
