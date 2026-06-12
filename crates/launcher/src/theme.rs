//! Y2K visual system: indigo plastic, brushed chrome, glacier glass.
//!
//! Everything here is painted procedurally with egui primitives — the
//! launcher ships zero image assets. The palette keys off the launch-era
//! hardware: translucent indigo shell, silver trim, electric accents.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Mesh, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    Vec2,
};

pub const INK: Color32 = Color32::from_rgb(0x10, 0x12, 0x2c); // midnight indigo
pub const DEEP: Color32 = Color32::from_rgb(0x22, 0x26, 0x58); // dusk indigo
pub const INDIGO: Color32 = Color32::from_rgb(0x3f, 0x44, 0x9e); // console shell
pub const VIOLET: Color32 = Color32::from_rgb(0x6f, 0x66, 0xdd); // glossy plastic
pub const CYAN: Color32 = Color32::from_rgb(0x7d, 0xe8, 0xff); // electric accent
pub const LIME: Color32 = Color32::from_rgb(0xc9, 0xf5, 0x4b); // hi-vis accent
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xc4, 0x00); // hazard stripe
pub const SILVER_HI: Color32 = Color32::from_rgb(0xee, 0xf1, 0xf8); // chrome light
pub const SILVER: Color32 = Color32::from_rgb(0xc4, 0xcb, 0xdb); // chrome mid
pub const SILVER_LO: Color32 = Color32::from_rgb(0x8e, 0x97, 0xb0); // chrome shade

pub fn white(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, alpha)
}

pub fn black(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(0, 0, 0, alpha)
}

fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// Global egui style: dark indigo base so our painted chrome sits on it.
pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.override_text_color = Some(SILVER_HI);
    v.panel_fill = INK;
    v.window_fill = DEEP;
    v.selection.bg_fill = tint(VIOLET, 160);
    v.selection.stroke = Stroke::new(1.0, CYAN);
    v.widgets.inactive.bg_fill = tint(INDIGO, 180);
    v.widgets.inactive.weak_bg_fill = tint(INDIGO, 120);
    v.widgets.hovered.bg_fill = tint(VIOLET, 200);
    v.widgets.hovered.weak_bg_fill = tint(VIOLET, 140);
    v.widgets.active.bg_fill = VIOLET;
    v.widgets.open.weak_bg_fill = tint(VIOLET, 140);
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    ctx.set_global_style(style);
}

/// Axis-aligned vertical gradient (sharp-cornered; used for full-bleed fills).
pub fn vgrad(p: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(rect.left_top(), top);
    m.colored_vertex(rect.right_top(), top);
    m.colored_vertex(rect.right_bottom(), bottom);
    m.colored_vertex(rect.left_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(m);
}

/// Soft radial glow: stacked fading circles (cheap, resolution-independent).
pub fn glow(p: &egui::Painter, center: Pos2, radius: f32, color: Color32, alpha: u8) {
    const STEPS: u8 = 6;
    for i in 0..STEPS {
        let f = 1.0 - i as f32 / STEPS as f32;
        p.circle_filled(
            center,
            radius * f,
            tint(color, (alpha as f32 / STEPS as f32) as u8),
        );
    }
}

/// Chrome orb with an upper-left specular highlight — the Y2K bullet point.
pub fn orb(p: &egui::Painter, center: Pos2, r: f32, color: Color32) {
    p.circle_filled(center + Vec2::new(0.0, r * 0.18), r, black(70)); // soft shadow
    p.circle_filled(center, r, color);
    let mut m = Mesh::default();
    let top = center - Vec2::new(0.0, r);
    m.colored_vertex(Pos2::new(center.x - r, top.y), white(0));
    m.colored_vertex(Pos2::new(center.x + r, top.y), white(0));
    m.colored_vertex(center + Vec2::new(r, 0.0), white(90));
    m.colored_vertex(center - Vec2::new(r, 0.0), white(90));
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(m);
    // highlight dot
    p.circle_filled(
        center + Vec2::new(-r * 0.35, -r * 0.4),
        r * 0.28,
        white(190),
    );
    p.circle_stroke(center, r, Stroke::new(1.0, black(80)));
}

/// Full-window backdrop: indigo gradient, tech grid, swoosh arcs, drifting
/// orbs, and a pulsing aurora glow. `t` is wall time in seconds.
pub fn paint_background(ui: &Ui, t: f64) {
    let rect = ui.max_rect();
    let p = ui.painter();
    vgrad(p, rect, DEEP, INK);

    // tech grid
    let step = 26.0;
    let mut x = rect.left();
    while x < rect.right() {
        p.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, white(4)),
        );
        x += step;
    }
    let mut y = rect.top();
    while y < rect.bottom() {
        p.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, white(5)),
        );
        y += step;
    }

    // aurora glow, gently breathing
    let pulse = (0.5 + 0.5 * (t * 0.8).sin()) as f32;
    glow(
        p,
        Pos2::new(
            rect.right() - rect.width() * 0.18,
            rect.top() + rect.height() * 0.12,
        ),
        rect.width() * 0.30,
        CYAN,
        (26.0 + 14.0 * pulse) as u8,
    );
    glow(
        p,
        Pos2::new(
            rect.left() + rect.width() * 0.1,
            rect.bottom() - rect.height() * 0.08,
        ),
        rect.width() * 0.22,
        VIOLET,
        30,
    );

    // swoosh arcs: big offscreen-centered circles, thin chrome strokes
    let c1 = Pos2::new(
        rect.center().x - rect.width() * 0.55,
        rect.bottom() + rect.height() * 0.9,
    );
    let c2 = Pos2::new(
        rect.center().x + rect.width() * 0.7,
        rect.top() - rect.height() * 0.75,
    );
    p.circle_stroke(c1, rect.width() * 1.05, Stroke::new(2.0, white(14)));
    p.circle_stroke(c1, rect.width() * 1.12, Stroke::new(1.0, white(9)));
    p.circle_stroke(c2, rect.width() * 0.95, Stroke::new(2.0, tint(CYAN, 16)));

    // drifting chrome orbs
    for (i, (fx, fy, r)) in [(0.16, 0.22, 7.0), (0.84, 0.62, 5.0), (0.32, 0.82, 4.0)]
        .iter()
        .enumerate()
    {
        let dx = ((t * 0.31 + i as f64 * 2.1).sin() * 9.0) as f32;
        let dy = ((t * 0.23 + i as f64 * 1.4).cos() * 6.0) as f32;
        let c = Pos2::new(
            rect.left() + rect.width() * fx + dx,
            rect.top() + rect.height() * fy + dy,
        );
        orb(p, c, *r, tint(SILVER, 120));
    }
}

/// Embossed chrome wordmark with a soft accent glow.
pub fn wordmark(p: &egui::Painter, pos: Pos2, text: &str, size: f32) {
    let font = FontId::proportional(size);
    p.text(
        pos + Vec2::new(1.0, 2.0),
        Align2::LEFT_CENTER,
        text,
        font.clone(),
        black(140),
    );
    p.text(
        pos + Vec2::new(0.0, -1.0),
        Align2::LEFT_CENTER,
        text,
        font.clone(),
        white(60),
    );
    p.text(pos, Align2::LEFT_CENTER, text, font, SILVER_HI);
}

/// Glossy plastic capsule button: vertical two-tone body, aqua highlight
/// band, bevel strokes, drop shadow. Selected = indigo plastic + cyan ring.
///
/// Every tone band is the full capsule clipped to a horizontal strip —
/// a band drawn as its own rounded rect gets its corner radius clamped
/// to the band's half-height by epaint, and those squarer corners poke
/// diagonally outside the capsule arc.
pub fn glossy_button(ui: &mut Ui, label: &str, selected: bool, size: Vec2) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let r = (rect.height() / 2.0).min(15.0) as u8;
    let cr = CornerRadius::same(r);
    let hovered = resp.hovered();
    let pressed = resp.is_pointer_button_down_on();

    let body = if selected {
        INDIGO
    } else if hovered {
        SILVER
    } else {
        SILVER_LO
    };
    let body_hi = if selected { VIOLET } else { SILVER_HI };

    // capsule fill clipped to the strip between y0 and y1
    let band = |shape: Rect, radius: CornerRadius, y0: f32, y1: f32, color: Color32| {
        p.with_clip_rect(Rect::from_min_max(
            Pos2::new(rect.left() - 4.0, y0),
            Pos2::new(rect.right() + 4.0, y1),
        ))
        .rect_filled(shape, radius, color);
    };

    p.rect_filled(
        rect.shrink(1.0).translate(Vec2::new(0.0, 2.0)),
        cr,
        black(70),
    );
    p.rect_filled(rect, cr, body);
    // upper-body tone
    band(
        rect,
        cr,
        rect.top(),
        rect.center().y + 2.0,
        tint(body_hi, if pressed { 90 } else { 200 }),
    );
    // aqua gloss band
    band(
        rect.shrink(3.0),
        CornerRadius::same(r.saturating_sub(3)),
        rect.top(),
        rect.top() + rect.height() * 0.45,
        white(if selected { 70 } else { 120 }),
    );
    // bottom shade
    band(
        rect,
        cr,
        rect.bottom() - rect.height() * 0.28,
        rect.bottom(),
        black(26),
    );

    let edge = if selected { CYAN } else { black(90) };
    p.rect_stroke(rect, cr, Stroke::new(1.0, edge), StrokeKind::Inside);
    if selected {
        p.rect_stroke(
            rect.shrink(2.0),
            CornerRadius::same(r.saturating_sub(2)),
            Stroke::new(1.5, tint(CYAN, 70)),
            StrokeKind::Inside,
        );
    }

    let fg = if selected { Color32::WHITE } else { INK };
    let font = FontId::proportional(rect.height().mul_add(0.38, 4.0).min(16.0));
    p.text(
        rect.center() + Vec2::new(0.0, 1.0),
        Align2::CENTER_CENTER,
        label,
        font.clone(),
        if selected { black(120) } else { white(110) },
    );
    p.text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    resp
}

/// Translucent "glacier glass" content panel.
pub fn glass_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(white(16))
        .stroke(Stroke::new(1.0, white(44)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(16.0)
}

/// Chrome strip (header/footer): silver gradient with bevel edges.
pub fn chrome_strip(p: &egui::Painter, rect: Rect) {
    vgrad(p, rect, SILVER_HI, SILVER_LO);
    vgrad(
        p,
        Rect::from_min_max(
            rect.min,
            Pos2::new(rect.max.x, rect.min.y + rect.height() * 0.45),
        ),
        white(150),
        white(20),
    );
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, black(120)),
    );
    p.line_segment(
        [rect.left_top(), rect.right_top()],
        Stroke::new(1.0, white(200)),
    );
}

/// Classic diagonal hazard stripes, clipped to `rect`. Currently between
/// users (the A/V video placeholder grew up); kept for the next
/// under-construction surface.
#[allow(dead_code)]
pub fn hazard_stripes(p: &egui::Painter, rect: Rect) {
    let clipped = p.with_clip_rect(rect);
    clipped.rect_filled(rect, CornerRadius::ZERO, tint(INK, 230));
    let w = 18.0;
    let mut x = rect.left() - rect.height();
    let mut on = true;
    while x < rect.right() + rect.height() {
        if on {
            let mut m = Mesh::default();
            m.colored_vertex(Pos2::new(x, rect.bottom()), AMBER);
            m.colored_vertex(Pos2::new(x + w, rect.bottom()), AMBER);
            m.colored_vertex(Pos2::new(x + w + rect.height(), rect.top()), AMBER);
            m.colored_vertex(Pos2::new(x + rect.height(), rect.top()), AMBER);
            m.add_triangle(0, 1, 2);
            m.add_triangle(0, 2, 3);
            clipped.add(m);
        }
        on = !on;
        x += w;
    }
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, black(150)),
        StrokeKind::Inside,
    );
}

/// Section heading: orb bullet + spaced uppercase chrome text.
pub fn section_title(ui: &mut Ui, title: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        orb(ui.painter(), rect.center(), 6.0, CYAN);
        let spaced: String = title.chars().flat_map(|c| [c, '\u{2009}']).collect();
        ui.label(
            egui::RichText::new(spaced.to_uppercase())
                .size(15.0)
                .strong()
                .color(SILVER_HI),
        );
    });
}
