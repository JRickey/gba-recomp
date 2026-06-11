//! Cartridge selection and launch: system file dialog, drag-and-drop,
//! and a persisted recently-played shelf.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    Vec2};

use crate::{platform, theme};

const MAX_RECENTS: usize = 10;

pub struct PlayScreen {
    recents: Vec<PathBuf>,
    /// Live play sessions we spawned. Reaped as they finish (so "now
    /// playing" always reflects a window that actually exists); any
    /// still alive are killed when the launcher exits — closing the
    /// launcher tears the whole product down.
    sessions: Vec<Session>,
    /// Transient launch/exit problem, shown until the next launch.
    notice: Option<String>,
}

/// What the child has told us via the `--status` protocol so far.
enum SessionState {
    /// Spawned; no status line yet.
    Starting,
    /// One-time translation in progress (0..=1).
    Building(f32),
    /// The game window is up.
    Playing,
}

enum SessionMsg {
    Building(f32),
    Playing,
    Stderr(String),
}

/// A running play process. State comes from reader threads draining the
/// child's pipes into `rx`; the UI thread never blocks on the child.
struct Session {
    name: String,
    child: std::process::Child,
    state: SessionState,
    /// Latest DEGRADED diagnostic — the product bar says surface these
    /// loudly, so it rides along in the launcher too.
    degraded: Option<String>,
    /// Last stderr line, kept as the failure message if the child dies.
    last_line: Option<String>,
    rx: std::sync::mpsc::Receiver<SessionMsg>,
}

impl Drop for PlayScreen {
    fn drop(&mut self) {
        for s in &mut self.sessions {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
    }
}

enum RowAction {
    None,
    Play,
    Forget,
}

impl PlayScreen {
    pub fn new() -> Self {
        Self { recents: load_recents(), sessions: Vec::new(), notice: None }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        // Absorb child status updates, then reap exited sessions.
        for s in &mut self.sessions {
            while let Ok(msg) = s.rx.try_recv() {
                match msg {
                    SessionMsg::Building(f) => s.state = SessionState::Building(f),
                    SessionMsg::Playing => s.state = SessionState::Playing,
                    SessionMsg::Stderr(line) => {
                        if line.starts_with("DEGRADED") {
                            s.degraded = Some(line.clone());
                        }
                        s.last_line = Some(line);
                    }
                }
            }
        }
        let notice = &mut self.notice;
        self.sessions.retain_mut(|s| match s.child.try_wait() {
            Ok(Some(code)) if !code.success() => {
                // Pick up any stderr that raced the exit, then report.
                while let Ok(msg) = s.rx.try_recv() {
                    if let SessionMsg::Stderr(line) = msg {
                        s.last_line = Some(line);
                    }
                }
                *notice = Some(format!(
                    "\u{2716} {} stopped: {}",
                    s.name,
                    s.last_line.as_deref().unwrap_or("no diagnostic")
                ));
                false
            }
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        });
        // Child state changes without UI input; poll while sessions live.
        if !self.sessions.is_empty() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }

        // files dropped anywhere count as an insert
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        for path in dropped {
            self.insert(path);
        }

        theme::glass_frame().show(ui, |ui| {
            ui.set_min_size(Vec2::new(ui.available_width(), ui.available_height()));
            theme::section_title(ui, "Cartridges");
            ui.add_space(6.0);

            // the big inviting button
            ui.vertical_centered(|ui| {
                let size = Vec2::new(260.0, 46.0);
                if theme::glossy_button(ui, "\u{25B6}  LOAD CARTRIDGE\u{2026}", true, size).clicked()
                {
                    if let Some(path) = platform::pick_rom() {
                        self.insert(path);
                    }
                }
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("or drop a .gba file anywhere in this window")
                        .size(11.0)
                        .color(theme::white(120)),
                );
            });

            ui.add_space(12.0);
            theme::section_title(ui, "Recently Played");
            ui.add_space(4.0);

            if self.recents.is_empty() {
                ui.label(
                    egui::RichText::new("Nothing here yet — your shelf is waiting.")
                        .italics()
                        .color(theme::white(110)),
                );
            }

            let mut play: Option<PathBuf> = None;
            let mut forget: Option<usize> = None;
            let footer = 26.0 + 32.0 * self.sessions.len() as f32;
            egui::ScrollArea::vertical()
                .max_height(ui.available_height() - footer)
                .show(ui, |ui| {
                    for (i, path) in self.recents.iter().enumerate() {
                        match recent_row(ui, path) {
                            RowAction::Play => play = Some(path.clone()),
                            RowAction::Forget => forget = Some(i),
                            RowAction::None => {}
                        }
                        ui.add_space(4.0);
                    }
                });
            if let Some(i) = forget {
                self.recents.remove(i);
                save_recents(&self.recents);
            }
            if let Some(path) = play {
                self.launch(&path);
            }

            // Session footer: build progress, live sessions, diagnostics.
            for s in &self.sessions {
                ui.add_space(2.0);
                match s.state {
                    SessionState::Starting => {
                        ui.label(
                            egui::RichText::new(format!("starting {}\u{2026}", s.name))
                                .size(11.0)
                                .color(theme::white(150)),
                        );
                    }
                    SessionState::Building(f) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "building {} \u{2014} one-time translation\u{2026}",
                                s.name
                            ))
                            .size(11.0)
                            .color(theme::CYAN),
                        );
                        ui.add(
                            egui::ProgressBar::new(f)
                                .desired_height(8.0)
                                .fill(theme::CYAN),
                        );
                    }
                    SessionState::Playing => {
                        ui.label(
                            egui::RichText::new(format!("\u{25B6} now playing: {}", s.name))
                                .size(11.0)
                                .color(theme::CYAN),
                        );
                    }
                }
                if let Some(d) = &s.degraded {
                    ui.label(egui::RichText::new(d).size(11.0).color(theme::AMBER));
                }
            }
            if let Some(n) = &self.notice {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(n).size(11.0).color(theme::AMBER));
            }
        });
    }

    fn insert(&mut self, path: PathBuf) {
        self.recents.retain(|p| *p != path);
        self.recents.insert(0, path);
        self.recents.truncate(MAX_RECENTS);
        save_recents(&self.recents);
    }

    fn launch(&mut self, path: &Path) {
        self.notice = None;
        match platform::launch(path) {
            Ok(mut child) => {
                let (tx, rx) = std::sync::mpsc::channel();
                if let Some(out) = child.stdout.take() {
                    read_status(out, tx.clone());
                }
                if let Some(err) = child.stderr.take() {
                    read_stderr(err, tx);
                }
                self.sessions.push(Session {
                    name: stem(path),
                    child,
                    state: SessionState::Starting,
                    degraded: None,
                    last_line: None,
                    rx,
                });
            }
            Err(e) => self.notice = Some(format!("\u{2716} {e}")),
        }
        // move to top of the shelf
        self.insert(path.to_path_buf());
    }
}

/// Drain the child's stdout on a thread: `STATUS` protocol lines update
/// the session; anything else is developer output, echoed to our stderr
/// so piping the child doesn't swallow its logs.
fn read_status(out: std::process::ChildStdout, tx: std::sync::mpsc::Sender<SessionMsg>) {
    use std::io::BufRead;
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let Some(rest) = line.strip_prefix("STATUS ") else {
                eprintln!("{line}");
                continue;
            };
            let msg = if rest == "playing" {
                Some(SessionMsg::Playing)
            } else if let Some(pct) = rest.strip_prefix("building ") {
                pct.trim().parse::<f32>().ok().map(|p| SessionMsg::Building(p / 100.0))
            } else {
                None
            };
            if let Some(msg) = msg {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        }
    });
}

/// Drain the child's stderr on a thread: echo it (it's the runtime's
/// diagnostic stream) and keep the session informed for DEGRADED and
/// exit reporting.
fn read_stderr(err: std::process::ChildStderr, tx: std::sync::mpsc::Sender<SessionMsg>) {
    use std::io::BufRead;
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
            eprintln!("{line}");
            if tx.send(SessionMsg::Stderr(line)).is_err() {
                break;
            }
        }
    });
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One shelf row: orb, name, location, hover PLAY chevron, forget cross.
fn recent_row(ui: &mut Ui, path: &Path) -> RowAction {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return RowAction::None;
    }
    let hovered = resp.hovered();
    let exists = path.is_file();
    let p = ui.painter();

    let cr = CornerRadius::same(9);
    p.rect_filled(rect, cr, theme::white(if hovered { 30 } else { 12 }));
    p.rect_stroke(rect, cr, Stroke::new(1.0, theme::white(if hovered { 90 } else { 30 })),
        StrokeKind::Inside);

    theme::orb(
        p,
        Pos2::new(rect.left() + 20.0, rect.center().y),
        7.0,
        if exists { theme::LIME } else { theme::SILVER_LO },
    );
    p.text(
        Pos2::new(rect.left() + 38.0, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        stem(path),
        FontId::proportional(14.0),
        theme::SILVER_HI,
    );
    let location = if exists {
        path.parent().map(|d| d.display().to_string()).unwrap_or_default()
    } else {
        "missing — was it moved?".to_string()
    };
    p.text(
        Pos2::new(rect.left() + 38.0, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        location,
        FontId::proportional(10.0),
        theme::white(110),
    );

    // forget cross (always present, quiet)
    let cross = Rect::from_center_size(
        Pos2::new(rect.right() - 18.0, rect.center().y),
        Vec2::splat(16.0),
    );
    let cross_resp = ui.interact(cross, resp.id.with("forget"), Sense::click());
    let cc = if cross_resp.hovered() { theme::AMBER } else { theme::white(90) };
    p.line_segment([cross.min + Vec2::splat(4.0), cross.max - Vec2::splat(4.0)],
        Stroke::new(1.6, cc));
    p.line_segment(
        [Pos2::new(cross.min.x + 4.0, cross.max.y - 4.0), Pos2::new(cross.max.x - 4.0, cross.min.y + 4.0)],
        Stroke::new(1.6, cc),
    );

    if hovered && exists {
        p.text(
            Pos2::new(rect.right() - 44.0, rect.center().y),
            Align2::RIGHT_CENTER,
            "PLAY \u{25B8}",
            FontId::proportional(12.0),
            theme::CYAN,
        );
    }

    if cross_resp.clicked() {
        RowAction::Forget
    } else if resp.clicked() && exists {
        RowAction::Play
    } else {
        RowAction::None
    }
}

fn recents_file() -> Option<PathBuf> {
    platform::config_dir().map(|d| d.join("recents.txt"))
}

fn load_recents() -> Vec<PathBuf> {
    let Some(f) = recents_file() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(f) else { return Vec::new() };
    text.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).take(MAX_RECENTS).collect()
}

fn save_recents(recents: &[PathBuf]) {
    if let Some(f) = recents_file() {
        let text: String =
            recents.iter().map(|p| format!("{}\n", p.display())).collect();
        let _ = std::fs::write(f, text);
    }
}
