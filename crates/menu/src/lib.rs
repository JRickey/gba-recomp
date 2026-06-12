//! In-game pause/settings overlay for the play runtime.
//!
//! This crate owns the overlay's state machine, a small embedded bitmap
//! font, and rgba compositing — everything the play loop needs to draw a
//! settings panel over the last composed frame and steer it with the same
//! keyboard/gamepad poll it already runs. It deliberately knows nothing
//! about windows, audio rings, or the emulator: the play loop feeds it
//! edge-triggered input and the live [`AvConfig`], and it returns the
//! actions to take (resume, quit, a setting changed) and draws the panel.
//!
//! The settings vocabulary mirrors the launcher's A/V surface, parsed
//! through the `screen` crate's enums so the two frontends never drift.

mod font;

use input_config::AvConfig;
use screen::{DisplayTarget, ResponseMode, ScreenKind};

// Overlay palette — drawn into the BGR555-derived frame, so these are
// plain sRGB-ish bytes. Tuned to echo the launcher's procedural chrome.
const PANEL_BG: [u8; 3] = [16, 18, 34];
const PANEL_EDGE: [u8; 3] = [0x6f, 0x66, 0xdd]; // launcher VIOLET
const TITLE_FG: [u8; 3] = [0xee, 0xf1, 0xf8]; // launcher SILVER_HI
const ROW_FG: [u8; 3] = [188, 198, 222];
const VALUE_FG: [u8; 3] = [0x7d, 0xe8, 0xff]; // launcher CYAN
const SELECT_FG: [u8; 3] = [255, 240, 150]; // amber, the selected row
const SELECT_BAR: [u8; 3] = [44, 50, 96];

/// One edge-triggered navigation event. The play loop debounces its own
/// keyboard/gamepad poll into these (a held direction yields one event on
/// the press edge, never a stream), so the menu logic stays stateless
/// about input timing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuInput {
    Up,
    Down,
    Left,
    Right,
    /// Activate the selected row (Enter / South).
    Confirm,
    /// Back out / close the menu (Escape / East).
    Cancel,
}

/// What the play loop should do after feeding input to the menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    /// Close the menu and continue the game.
    Resume,
    /// Save and quit.
    Quit,
    /// A persisted setting changed; the loop reapplies live state and
    /// writes `av.cfg`. The variant says which surface moved so the loop
    /// only rebuilds what it must.
    Changed(Changed),
}

/// Which A/V surface a [`MenuAction::Changed`] touched, so the play loop
/// rebuilds only the affected objects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Changed {
    /// `audio_enhanced` flipped.
    Audio,
    /// A video setting that rebuilds the color LUT / temporal / grid in
    /// place (screen model, darken, response, grid).
    Video,
    /// The output gamut — may need the GPU presenter recreated; the loop
    /// decides whether to apply now or on next launch.
    Gamut,
    /// `video_vsync` flipped; the loop retunes the presenter in place.
    Vsync,
}

/// The rows of the menu, in display order. Action rows (`Resume`,
/// `Quit`) and setting rows (everything else) share the list; setting
/// rows cycle a value on left/right/confirm.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Resume,
    Audio,
    Screen,
    Darken,
    Response,
    Grid,
    Gamut,
    Vsync,
    Quit,
}

impl Row {
    const ALL: [Row; 9] = [
        Row::Resume,
        Row::Audio,
        Row::Screen,
        Row::Darken,
        Row::Response,
        Row::Grid,
        Row::Gamut,
        Row::Vsync,
        Row::Quit,
    ];

    fn label(self) -> &'static str {
        match self {
            Row::Resume => "Resume",
            Row::Audio => "Enhanced Audio",
            Row::Screen => "Screen",
            Row::Darken => "Darken",
            Row::Response => "Response",
            Row::Grid => "Grid",
            Row::Gamut => "Gamut",
            Row::Vsync => "Vsync",
            Row::Quit => "Quit (saves)",
        }
    }

    /// Is this an action row (no value) rather than a setting row?
    fn is_action(self) -> bool {
        matches!(self, Row::Resume | Row::Quit)
    }
}

/// The pause-overlay state machine.
pub struct Menu {
    selected: usize,
    /// Whether the enhanced-audio toggle applies live or on next launch.
    /// The play loop reports this once at construction; the menu only
    /// surfaces it as a footnote on that row.
    audio_live: bool,
    /// Same, for the output gamut.
    gamut_live: bool,
}

impl Menu {
    /// Open a fresh menu with the first row selected. `audio_live` /
    /// `gamut_live` tell the menu whether those settings take effect
    /// immediately (it annotates the rows that don't).
    pub fn new(audio_live: bool, gamut_live: bool) -> Menu {
        Menu {
            selected: 0,
            audio_live,
            gamut_live,
        }
    }

    /// Reset selection to the top (called on each open so the cursor
    /// doesn't persist a stale position across pauses).
    pub fn reset(&mut self) {
        self.selected = 0;
    }

    /// Feed one navigation event. Mutates `av` for value changes and
    /// returns the action the play loop must take, if any. `Up`/`Down`
    /// move the cursor (no action). `Left`/`Right` cycle the selected
    /// setting. `Confirm` activates an action row or advances a setting.
    /// `Cancel` resumes.
    pub fn handle(&mut self, input: MenuInput, av: &mut AvConfig) -> Option<MenuAction> {
        let row = Row::ALL[self.selected];
        match input {
            MenuInput::Up => {
                self.selected = (self.selected + Row::ALL.len() - 1) % Row::ALL.len();
                None
            }
            MenuInput::Down => {
                self.selected = (self.selected + 1) % Row::ALL.len();
                None
            }
            MenuInput::Cancel => Some(MenuAction::Resume),
            MenuInput::Confirm => match row {
                Row::Resume => Some(MenuAction::Resume),
                Row::Quit => Some(MenuAction::Quit),
                // Confirm on a setting advances it like Right.
                _ => cycle(row, av, true).map(MenuAction::Changed),
            },
            MenuInput::Left => {
                if row.is_action() {
                    None
                } else {
                    cycle(row, av, false).map(MenuAction::Changed)
                }
            }
            MenuInput::Right => {
                if row.is_action() {
                    None
                } else {
                    cycle(row, av, true).map(MenuAction::Changed)
                }
            }
        }
    }

    /// Composite the overlay onto a composed `rgba` frame (`w*h`,
    /// row-major, RGBA bytes — the alpha channel is left at 255 and
    /// ignored by both present paths). Drawn entirely in the frame's own
    /// pixel space so it scales with the window at present time and works
    /// identically under the GPU presenter and the CPU blit.
    ///
    /// Palette echoes the launcher's Y2K chrome (violet edge, silver
    /// title, cyan values, amber selection) so the two settings surfaces
    /// read as one product.
    pub fn draw(&self, av: &AvConfig, rgba: &mut [[u8; 4]], w: usize, h: usize) {
        // Darken the whole frame so the panel reads regardless of scene.
        for px in rgba.iter_mut() {
            px[0] = (px[0] as u16 * 5 / 16) as u8;
            px[1] = (px[1] as u16 * 5 / 16) as u8;
            px[2] = (px[2] as u16 * 5 / 16) as u8;
        }

        // Panel geometry, in frame pixels. The GBA frame is 240x160; a
        // 1px font scale keeps the whole menu on-screen with room to
        // spare, and stays crisp at the window's integer scale.
        let pad = 6usize;
        let panel_x = 10usize;
        let panel_y = 8usize;
        let panel_w = w.saturating_sub(20).min(220);
        let line_h = font::H + 4;
        let n = Row::ALL.len();
        let panel_h = (pad * 2 + line_h + 6 + n * line_h + 6).min(h.saturating_sub(panel_y * 2));

        fill_rect(rgba, w, h, panel_x, panel_y, panel_w, panel_h, PANEL_BG);
        frame_rect(rgba, w, h, panel_x, panel_y, panel_w, panel_h, PANEL_EDGE);

        let tx = panel_x + pad;
        let mut ty = panel_y + pad;

        draw_text(rgba, w, h, tx, ty, "PAUSED", TITLE_FG, 1);
        ty += line_h;
        // Title rule, in the launcher's accent — ties the surfaces together.
        fill_rect(
            rgba,
            w,
            h,
            panel_x + pad,
            ty,
            panel_w - pad * 2,
            1,
            PANEL_EDGE,
        );
        ty += 6;

        for (i, row) in Row::ALL.iter().enumerate() {
            let sel = i == self.selected;
            if sel {
                // Selection highlight bar behind the row.
                fill_rect(
                    rgba,
                    w,
                    h,
                    panel_x + 2,
                    ty - 2,
                    panel_w - 4,
                    line_h,
                    SELECT_BAR,
                );
            }
            let fg = if sel { SELECT_FG } else { ROW_FG };
            let cursor = if sel { ">" } else { " " };
            draw_text(rgba, w, h, tx, ty, cursor, fg, 1);

            let mut label = row.label().to_string();
            // Annotate the rows that only apply on next launch.
            if matches!(row, Row::Audio) && !self.audio_live {
                label.push_str(" (restart)");
            }
            if matches!(row, Row::Gamut) && !self.gamut_live {
                label.push_str(" (restart)");
            }

            // The value (right-aligned) is laid out first so the label can
            // be clipped to never collide with it — a long label + wide
            // value must read cleanly, not overlap into garbage.
            let val = value_of(*row, av);
            let glyph = font::W + 1;
            let lx = tx + glyph;
            let val_left = val
                .as_ref()
                .map(|v| panel_x + panel_w - pad - v.chars().count() * glyph);
            let label_cap = match val_left {
                Some(vl) if vl > lx => (vl - lx) / glyph,
                Some(_) => 0,
                None => (panel_x + panel_w - pad).saturating_sub(lx) / glyph,
            };
            if label.chars().count() > label_cap {
                label.truncate(label_cap.saturating_sub(1).min(label.len()));
            }
            draw_text(rgba, w, h, lx, ty, &label, fg, 1);
            if let (Some(v), Some(vx)) = (val, val_left) {
                draw_text(
                    rgba,
                    w,
                    h,
                    vx,
                    ty,
                    &v,
                    if sel { SELECT_FG } else { VALUE_FG },
                    1,
                );
            }
            ty += line_h;
        }
    }
}

/// The display value for a setting row (`None` for action rows).
fn value_of(row: Row, av: &AvConfig) -> Option<String> {
    Some(match row {
        Row::Resume | Row::Quit => return None,
        Row::Audio => if av.audio_enhanced { "ON" } else { "OFF" }.to_string(),
        Row::Screen => screen_kind(av).name().to_uppercase(),
        Row::Darken => av.screen_darken.to_uppercase(),
        Row::Response => response_mode(av).name().to_uppercase(),
        Row::Grid => av.grid.to_uppercase(),
        Row::Gamut => display_target(av).name().to_uppercase(),
        Row::Vsync => if av.video_vsync { "ON" } else { "OFF" }.to_string(),
    })
}

/// Cycle the selected setting forward (`forward`) or backward and report
/// which surface moved. Returns `None` for action rows.
fn cycle(row: Row, av: &mut AvConfig, forward: bool) -> Option<Changed> {
    match row {
        Row::Resume | Row::Quit => None,
        Row::Audio => {
            av.audio_enhanced = !av.audio_enhanced;
            Some(Changed::Audio)
        }
        Row::Screen => {
            let cur = screen_kind(av);
            let next = step_enum(&ScreenKind::ALL, cur, forward);
            av.screen = next.name().to_string();
            Some(Changed::Video)
        }
        Row::Darken => {
            av.screen_darken = step_knob(&av.screen_darken, forward);
            Some(Changed::Video)
        }
        Row::Response => {
            let cur = response_mode(av);
            let next = step_enum(&ResponseMode::ALL, cur, forward);
            av.response = next.name().to_string();
            Some(Changed::Video)
        }
        Row::Grid => {
            av.grid = step_knob(&av.grid, forward);
            Some(Changed::Video)
        }
        Row::Gamut => {
            let cur = display_target(av);
            let next = step_enum(&DisplayTarget::ALL, cur, forward);
            av.display_gamut = next.name().to_string();
            Some(Changed::Gamut)
        }
        Row::Vsync => {
            av.video_vsync = !av.video_vsync;
            Some(Changed::Vsync)
        }
    }
}

/// A 0..1 knob string cycled through `auto` and discrete tenths. The
/// launcher exposes these as continuous sliders; the overlay steps them
/// so a d-pad can reach every useful value.
fn step_knob(cur: &str, forward: bool) -> String {
    // Ordered ring: auto, 0.0, 0.1, ... 1.0.
    let mut steps: Vec<String> = vec!["auto".to_string()];
    for i in 0..=10 {
        steps.push(format!("{:.1}", i as f32 / 10.0));
    }
    // Find the closest current entry (parse, else "auto").
    let idx = match AvConfig::knob(cur) {
        None => 0,
        Some(v) => {
            let q = (v.clamp(0.0, 1.0) * 10.0).round() as usize;
            (q + 1).min(steps.len() - 1)
        }
    };
    let next = if forward {
        (idx + 1) % steps.len()
    } else {
        (idx + steps.len() - 1) % steps.len()
    };
    steps[next].clone()
}

/// Step through a fixed enum table, wrapping.
fn step_enum<T: Copy + PartialEq>(all: &[T], cur: T, forward: bool) -> T {
    let i = all.iter().position(|&x| x == cur).unwrap_or(0);
    let n = all.len();
    if forward {
        all[(i + 1) % n]
    } else {
        all[(i + n - 1) % n]
    }
}

fn screen_kind(av: &AvConfig) -> ScreenKind {
    ScreenKind::by_name(&av.screen).unwrap_or(ScreenKind::Frontlit)
}

fn response_mode(av: &AvConfig) -> ResponseMode {
    ResponseMode::by_name(&av.response).unwrap_or(ResponseMode::Smart)
}

fn display_target(av: &AvConfig) -> DisplayTarget {
    DisplayTarget::by_name(&av.display_gamut).unwrap_or(DisplayTarget::Auto)
}

// ── rgba drawing primitives ─────────────────────────────────────────

fn put(rgba: &mut [[u8; 4]], w: usize, h: usize, x: usize, y: usize, c: [u8; 3]) {
    if x < w && y < h {
        let p = &mut rgba[y * w + x];
        *p = [c[0], c[1], c[2], 255];
    }
}

/// Solid filled rectangle, clipped to the frame.
fn fill_rect(
    rgba: &mut [[u8; 4]],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    c: [u8; 3],
) {
    for yy in y..(y + rh).min(h) {
        for xx in x..(x + rw).min(w) {
            rgba[yy * w + xx] = [c[0], c[1], c[2], 255];
        }
    }
}

/// One-pixel border around a rectangle.
fn frame_rect(
    rgba: &mut [[u8; 4]],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    c: [u8; 3],
) {
    for xx in x..(x + rw).min(w) {
        put(rgba, w, h, xx, y, c);
        if y + rh >= 1 {
            put(rgba, w, h, xx, (y + rh - 1).min(h - 1), c);
        }
    }
    for yy in y..(y + rh).min(h) {
        put(rgba, w, h, x, yy, c);
        if x + rw >= 1 {
            put(rgba, w, h, (x + rw - 1).min(w - 1), yy, c);
        }
    }
}

/// Draw a string with the embedded font at integer `scale` (1 = native
/// 8x8 cells). Glyphs advance by one extra pixel for readability.
fn draw_text(
    rgba: &mut [[u8; 4]],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    s: &str,
    c: [u8; 3],
    scale: usize,
) {
    let mut cx = x;
    for ch in s.chars() {
        let g = font::glyph(ch);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..font::W {
                if bits & (0x80 >> col) != 0 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            put(rgba, w, h, cx + col * scale + sx, y + row * scale + sy, c);
                        }
                    }
                }
            }
        }
        cx += (font::W + 1) * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enhanced_audio_toggles_and_reports_change() {
        let mut av = AvConfig::default();
        assert!(!av.audio_enhanced);
        let mut menu = Menu::new(true, true);
        // Row index 1 is the audio row.
        menu.selected = 1;
        let act = menu.handle(MenuInput::Right, &mut av);
        assert_eq!(act, Some(MenuAction::Changed(Changed::Audio)));
        assert!(av.audio_enhanced);
    }

    #[test]
    fn screen_cycles_through_known_models() {
        let mut av = AvConfig::default();
        av.screen = "frontlit".into();
        let mut menu = Menu::new(true, true);
        menu.selected = 2; // screen row
        menu.handle(MenuInput::Right, &mut av);
        assert!(ScreenKind::by_name(&av.screen).is_some());
        // A full cycle returns to the start.
        for _ in 0..ScreenKind::ALL.len() {
            menu.handle(MenuInput::Right, &mut av);
        }
        assert!(ScreenKind::by_name(&av.screen).is_some());
    }

    #[test]
    fn resume_and_quit_actions() {
        let mut av = AvConfig::default();
        let mut menu = Menu::new(true, true);
        menu.selected = 0;
        assert_eq!(
            menu.handle(MenuInput::Confirm, &mut av),
            Some(MenuAction::Resume)
        );
        assert_eq!(
            menu.handle(MenuInput::Cancel, &mut av),
            Some(MenuAction::Resume)
        );
        menu.selected = Row::ALL.len() - 1; // Quit
        assert_eq!(
            menu.handle(MenuInput::Confirm, &mut av),
            Some(MenuAction::Quit)
        );
    }

    #[test]
    fn knob_steps_wrap_through_auto() {
        assert_eq!(step_knob("auto", true), "0.0");
        assert_eq!(step_knob("0.0", false), "auto");
        assert_eq!(step_knob("1.0", true), "auto");
        assert_eq!(step_knob("0.5", true), "0.6");
    }

    #[test]
    fn navigation_wraps() {
        let mut av = AvConfig::default();
        let mut menu = Menu::new(true, true);
        menu.selected = 0;
        menu.handle(MenuInput::Up, &mut av);
        assert_eq!(menu.selected, Row::ALL.len() - 1);
        menu.handle(MenuInput::Down, &mut av);
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn draw_does_not_panic_and_marks_panel() {
        let av = AvConfig::default();
        let menu = Menu::new(false, false);
        let mut rgba = vec![[10u8, 10, 10, 255]; 240 * 160];
        menu.draw(&av, &mut rgba, 240, 160);
        // The darkening pass alone changes the buffer.
        assert!(rgba.iter().any(|p| p[0] != 10 || p[1] != 10 || p[2] != 10));
    }
}
