//! Interactive `play` runtime: a winit-driven window with the GPU presenter
//! (or a software fallback), keyboard + gamepad input, the in-game menu, and
//! the audio-clock pacing loop. Extracted from `main.rs` as the one cohesive
//! "interactive shell" unit; everything it needs from the CLI/runtime core
//! (translation, machine construction, audio rings, screen config) it reaches
//! through `crate::` — this module is a child of the crate root and so may
//! use the root's items directly.
//!
//! Pacing principle (load-bearing, see `screen::present` docs): emulation
//! speed is owned by the audio clock, never the display. winit 0.30 is
//! callback-driven, so the former `while window.is_open()` body lives in
//! [`ApplicationHandler::about_to_wait`] under `ControlFlow::Poll`, and the
//! frame is presented *directly* there the instant it is produced — never
//! gated on `RedrawRequested`/vsync, which would couple game speed to the
//! panel.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gba_core::{Bus as _, Machine};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use recomp_core::fnv64;

use crate::{
    apply_av_change, arm_audio_hle, build_gilrs, ensure_native, load_bios_file, load_native,
    make_machine, pad_pressed, print_fallback_census, record_labels, rom_sha256, run_frame_native,
    start_audio, status_line, validate_rom, write_sav, AudioStreams, BlockTable, FALLBACK_COLLECT,
    NO_INTERP, RING_TARGET,
};

/// Per-frame down-state of the host-menu navigation inputs, debounced against
/// the previous frame to produce rising-edge events. Kept separate from the
/// GBA key state — menu input never leaks to the game.
#[derive(Clone, Copy, Default)]
struct NavEdges {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    confirm: bool,
    /// Back out of the menu (Escape / pad East). Only consulted while the
    /// menu is open — East is the player's B button during gameplay, so it
    /// must never open the menu.
    cancel: bool,
    /// Open the menu from gameplay: keyboard Escape, or a dedicated pad
    /// control that doesn't collide with any GBA button (the guide/Mode
    /// button).
    open: bool,
}

/// Canonical key name (see input-config) to a winit *physical* key. Physical
/// (scancode-position) keys keep a binding in the same spot regardless of
/// QWERTY/AZERTY — the gamepad-style "this key here" model the old minifb
/// mapping had, which also sidesteps layout-dependent defaults.
fn winit_key(name: &str) -> Option<KeyCode> {
    use KeyCode::*;
    Some(match name {
        "A" => KeyA,
        "B" => KeyB,
        "C" => KeyC,
        "D" => KeyD,
        "E" => KeyE,
        "F" => KeyF,
        "G" => KeyG,
        "H" => KeyH,
        "I" => KeyI,
        "J" => KeyJ,
        "K" => KeyK,
        "L" => KeyL,
        "M" => KeyM,
        "N" => KeyN,
        "O" => KeyO,
        "P" => KeyP,
        "Q" => KeyQ,
        "R" => KeyR,
        "S" => KeyS,
        "T" => KeyT,
        "U" => KeyU,
        "V" => KeyV,
        "W" => KeyW,
        "X" => KeyX,
        "Y" => KeyY,
        "Z" => KeyZ,
        "0" => Digit0,
        "1" => Digit1,
        "2" => Digit2,
        "3" => Digit3,
        "4" => Digit4,
        "5" => Digit5,
        "6" => Digit6,
        "7" => Digit7,
        "8" => Digit8,
        "9" => Digit9,
        "Up" => ArrowUp,
        "Down" => ArrowDown,
        "Left" => ArrowLeft,
        "Right" => ArrowRight,
        "Enter" => Enter,
        "Space" => Space,
        "Tab" => Tab,
        "Backspace" => Backspace,
        "LeftShift" => ShiftLeft,
        "RightShift" => ShiftRight,
        _ => return None,
    })
}

/// Crossing the pause boundary, clear the audio defect counters so the
/// drained-ring underruns during pause don't false-fire the DEGRADED alarm
/// next report window.
fn reset_audio_counters(streams: &Arc<Mutex<AudioStreams>>) {
    let mut st = streams.lock().unwrap();
    st.drops = 0;
    st.holds = 0;
    st.clipped = 0;
}

/// All play-session state. Pre-window state is built in [`cmd_play`] and moved
/// in; the window, GPU presenter, and software-blit surface are created in
/// [`ApplicationHandler::resumed`] (window creation is only valid once the
/// event loop is active).
struct PlayApp {
    // Core machine + persistence.
    machine: Machine,
    sav_path: String,
    sav_seen: Option<u64>,
    record: Option<(std::fs::File, u16)>,
    native: Option<(libloading::Library, BlockTable)>,

    // Audio.
    streams: Arc<Mutex<AudioStreams>>,
    stream: Option<cpal::Stream>,

    // Input.
    icfg: input_config::InputConfig,
    key_pairs: Vec<(KeyCode, u16)>,
    pad: Option<gilrs::Gilrs>,
    /// Physical keys currently held, reconstructed from key press/release
    /// events — winit's event-driven equivalent of minifb's `is_key_down`.
    held: HashSet<KeyCode>,

    // Screen simulation (rebuilt live by the menu).
    av: input_config::AvConfig,
    screen_kind: screen::ScreenKind,
    /// Configured gamut (auto/srgb/display-p3), used for the macOS layer tag.
    display_target: screen::DisplayTarget,
    /// Primaries the LUT actually encodes for, resolved once from
    /// `display_target` + OS detection (may be the measured panel gamut).
    lut_primaries: screen::color::Primaries,
    lut: screen::ColorLut,
    response: screen::ResponseMode,
    temporal: screen::Temporal,
    grid_params: screen::present::GridParams,
    rgba: Vec<[u8; 4]>,

    // Window + presentation (created in `resumed`).
    title: String,
    status: bool,
    window: Option<Arc<Window>>,
    presenter: Option<screen::present::Presenter>,
    sb_context: Option<softbuffer::Context<Arc<Window>>>,
    sb_surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,

    // Menu / pause.
    menu_enabled: bool,
    menu: menu::Menu,
    paused: bool,
    cursor_visible: bool,
    suppress: u16,
    nav_prev: NavEdges,

    // Perf + audio watchdog.
    show_stats: bool,
    emu_ema_ms: f64,
    frames_run: u64,
    slow_warned: bool,
    native_run: u64,
    fallback_run: u64,
    primed: bool,
    next_frame: Instant,
    audio_watch: Instant,
    audio_consumed_last: u64,
    audio_stalls: u8,
    audio_retry: Option<Instant>,

    /// A fatal error raised inside a callback; surfaced after `run_app`.
    error: Option<String>,
}

impl PlayApp {
    /// Present `self.rgba` at the window's current size: the GPU grid/scaling
    /// pass when available, otherwise the software letterbox blit (still fully
    /// color-corrected, no grid). Cloning the `Arc` avoids borrowing
    /// `self.window` while `self.presenter`/`self.sb_surface` are mutated.
    fn present_rgba(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let phys = (size.width.max(1), size.height.max(1));
        if let Some(p) = self.presenter.as_mut() {
            match p.present(&self.rgba, phys, &self.grid_params) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("video: GPU presenter failed ({e}); basic scaling active");
                    self.presenter = None;
                }
            }
        }
        self.present_software(&window, phys);
    }

    /// Software fallback (no GPU / presenter lost): nearest-neighbor, aspect-
    /// correct letterbox of the 240x160 frame into the window via softbuffer,
    /// matching the GPU presenter's centering. Lazily creates the surface.
    fn present_software(&mut self, window: &Arc<Window>, (w, h): (u32, u32)) {
        if self.sb_surface.is_none() {
            match softbuffer::Context::new(window.clone()) {
                Ok(ctx) => match softbuffer::Surface::new(&ctx, window.clone()) {
                    Ok(surf) => {
                        self.sb_context = Some(ctx);
                        self.sb_surface = Some(surf);
                    }
                    Err(e) => {
                        eprintln!("video: software surface unavailable ({e})");
                        return;
                    }
                },
                Err(e) => {
                    eprintln!("video: software context unavailable ({e})");
                    return;
                }
            }
        }
        let Some(surf) = self.sb_surface.as_mut() else {
            return;
        };
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else {
            return;
        };
        if let Err(e) = surf.resize(nw, nh) {
            eprintln!("video: software resize failed ({e})");
            return;
        }
        let mut buffer = match surf.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("video: software buffer unavailable ({e})");
                return;
            }
        };
        let (sw, sh) = (240u32, 160u32);
        let scale = ((w as f32) / (sw as f32)).min((h as f32) / (sh as f32));
        let dw = ((sw as f32) * scale) as u32;
        let dh = ((sh as f32) * scale) as u32;
        let ox = w.saturating_sub(dw) / 2;
        let oy = h.saturating_sub(dh) / 2;
        for px in buffer.iter_mut() {
            *px = 0;
        }
        for y in 0..dh.min(h) {
            let sy = (((y as f32) / scale) as u32).min(sh - 1);
            let drow = ((oy + y) * w) as usize;
            let srow = (sy * sw) as usize;
            for x in 0..dw.min(w) {
                let sx = (((x as f32) / scale) as u32).min(sw - 1);
                let s = self.rgba[srow + sx as usize];
                buffer[drow + (ox + x) as usize] =
                    (s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32;
            }
        }
        let _ = buffer.present();
    }

    /// Gather this frame's down-state into the active-low GBA `keys` mask and
    /// the host-menu [`NavEdges`], from the held-key set plus the gamepad.
    fn gather_input(&mut self) -> (u16, NavEdges) {
        // KEYINPUT is active-low.
        let mut keys = 0x3FFu16;
        for (code, bit) in &self.key_pairs {
            if self.held.contains(code) {
                keys &= !bit;
            }
        }
        // Host-menu navigation down-states. On the keyboard Escape both opens
        // and backs out (it never aliases a GBA key).
        let mut nav = NavEdges {
            up: self.held.contains(&KeyCode::ArrowUp),
            down: self.held.contains(&KeyCode::ArrowDown),
            left: self.held.contains(&KeyCode::ArrowLeft),
            right: self.held.contains(&KeyCode::ArrowRight),
            confirm: self.held.contains(&KeyCode::Enter),
            cancel: self.held.contains(&KeyCode::Escape),
            open: self.held.contains(&KeyCode::Escape),
        };
        if let Some(g) = self.pad.as_mut() {
            while g.next_event().is_some() {}
            let chosen = g
                .gamepads()
                .find(|(_, gp)| gp.name() == self.icfg.gamepad_name)
                .or_else(|| g.gamepads().next());
            if let Some((_, gp)) = chosen {
                let dz = self.icfg.stick_deadzone;
                for b in input_config::Button::ALL {
                    if pad_pressed(&gp, &self.icfg.pads[b.index()], dz) {
                        keys &= !b.bit();
                    }
                }
                // A configured stick drives the GBA d-pad per dpad_source (the
                // physical d-pad still works through the bindings above). Y is
                // up-positive in gilrs.
                let src = self.icfg.dpad_source;
                if src.uses_left_stick() || src.uses_right_stick() {
                    let (ax, ay) = if src.uses_right_stick() {
                        (gilrs::Axis::RightStickX, gilrs::Axis::RightStickY)
                    } else {
                        (gilrs::Axis::LeftStickX, gilrs::Axis::LeftStickY)
                    };
                    let (x, y) = (gp.value(ax), gp.value(ay));
                    if x > dz {
                        keys &= !(1 << 4);
                    } // Right
                    if x < -dz {
                        keys &= !(1 << 5);
                    } // Left
                    if y > dz {
                        keys &= !(1 << 6);
                    } // Up
                    if y < -dz {
                        keys &= !(1 << 7);
                    } // Down
                }
                // Menu nav from the pad: d-pad and sticks move the cursor;
                // South confirms, East backs out. These steer the menu only
                // while it is open, so reusing the gameplay face buttons here
                // is safe.
                use gilrs::{Axis, Button as Gb};
                nav.up |= gp.is_pressed(Gb::DPadUp) || gp.value(Axis::LeftStickY) > dz;
                nav.down |= gp.is_pressed(Gb::DPadDown) || gp.value(Axis::LeftStickY) < -dz;
                nav.left |= gp.is_pressed(Gb::DPadLeft) || gp.value(Axis::LeftStickX) < -dz;
                nav.right |= gp.is_pressed(Gb::DPadRight) || gp.value(Axis::LeftStickX) > dz;
                nav.confirm |= gp.is_pressed(Gb::South);
                nav.cancel |= gp.is_pressed(Gb::East);
                // Opening from the pad uses the guide/Mode button — the one
                // control no GBA button maps to. (Keyboard Escape always works
                // too.) A chord like Start+Select is deliberately NOT a
                // default: those are real in-game buttons.
                nav.open |= gp.is_pressed(Gb::Mode);
            }
        }
        (keys, nav)
    }

    /// One emulated frame plus its audio handoff to the rings. Counts native
    /// vs fallback dispatch, advances the screen's temporal response, surfaces
    /// the realtime / audio-defect alarms, and flushes backup media.
    fn run_one_frame(&mut self) {
        let emu_t0 = Instant::now();
        match &self.native {
            Some((_lib, table)) => {
                let mptr = &mut self.machine as *mut Machine as *mut std::ffi::c_void;
                let (n, f) = run_frame_native(&mut self.machine, table, mptr, 5_000_000);
                self.native_run += n;
                self.fallback_run += f;
            }
            None => self.machine.run_frame(5_000_000),
        }
        // Advance the screen's temporal response once per EMULATED frame —
        // never per presented frame, or flicker-transparency titles lock onto
        // one phase (the classic parity bug).
        self.temporal.push(&self.machine.bus.framebuffer, &self.lut);

        // Realtime alarm: the frame budget is 16.7 ms. If smoothed emulation
        // cost approaches it, the product promise is broken — say so once.
        let dt_ms = emu_t0.elapsed().as_secs_f64() * 1e3;
        self.emu_ema_ms = if self.frames_run == 0 {
            dt_ms
        } else {
            self.emu_ema_ms * 0.95 + dt_ms * 0.05
        };
        self.frames_run += 1;
        if !self.slow_warned && self.frames_run > 120 && self.emu_ema_ms > 15.0 {
            self.slow_warned = true;
            eprintln!(
                "DEGRADED: emulation averaging {:.1} ms/frame (budget 16.7 ms) — below native speed",
                self.emu_ema_ms
            );
        }

        // Live perf readout in the title, once a second. Developer tooling —
        // hidden from the release out-of-box experience unless requested.
        if self.show_stats && self.frames_run % 60 == 0 {
            let total = self.native_run + self.fallback_run;
            let share = if total == 0 {
                0.0
            } else {
                self.native_run as f64 * 100.0 / total as f64
            };
            if let Some(w) = self.window.as_ref() {
                w.set_title(&format!(
                    "{} · {:.2} ms/frame ({:.1}x headroom) · native {share:.0}%",
                    self.title,
                    self.emu_ema_ms,
                    16.7 / self.emu_ema_ms.max(0.01),
                ));
            }
            self.native_run = 0;
            self.fallback_run = 0;
        }

        // Audio defect surfacing: ring drops and underrun holds are silent
        // corruption — say so, with numbers, per the product bar.
        if self.frames_run % 300 == 0 {
            let (d, h, c) = {
                let mut st = self.streams.lock().unwrap();
                let v = (st.drops, st.holds, st.clipped);
                st.drops = 0;
                st.holds = 0;
                st.clipped = 0;
                v
            };
            if d + h > 0 {
                eprintln!(
                    "DEGRADED: audio ring dropped {d} / held {h} sample pairs in the last 5 s"
                );
            }
            // Soft-knee engagement is canon clipping handled better, not a
            // defect — visible only in developer stats.
            if self.show_stats && c > 0 {
                eprintln!("audio: soft clip engaged on {c} sample pairs in the last 5 s");
            }
        }

        // Flush backup media periodically when it changed, so an external
        // teardown (launcher kills us) can't lose more than ~5 s of progress.
        if self.frames_run % 300 == 0 {
            if let Some(data) = self.machine.bus.save_data() {
                let h = fnv64(data);
                if self.sav_seen != Some(h) && write_sav(&self.sav_path, data).is_ok() {
                    self.sav_seen = Some(h);
                }
            }
        }

        self.handoff_audio();
    }

    /// Hand this frame's audio taps to the consumer rings, refresh the routing
    /// snapshot, and count anything dropped. With no consumer, the taps are
    /// discarded (not a defect — reported when the device died) so the rings
    /// can't overflow into permanent DEGRADED spam.
    fn handoff_audio(&mut self) {
        let bus = &mut self.machine.bus;
        if self.stream.is_none() {
            bus.audio_buf.clear();
            bus.psg_tap.clear();
            bus.hle_tap.clear();
            bus.fifo_tap[0].clear();
            bus.fifo_tap[1].clear();
            return;
        }
        let cnt_h = bus.read16(0x0400_0082);
        let mut st = self.streams.lock().unwrap();
        st.route = [(cnt_h >> 8 & 3) as u8, (cnt_h >> 12 & 3) as u8];
        st.vol_half = [cnt_h & 4 == 0, cnt_h & 8 == 0];
        // Faithful ring: always produced (the menu may crossfade to it at any
        // moment), so it must always be primed.
        for pair in bus.audio_buf.chunks_exact(2) {
            if st.mixed.len() < 65536 {
                st.mixed.push_back(pair[0]);
                st.mixed.push_back(pair[1]);
            } else {
                st.drops += 1;
            }
        }
        bus.audio_buf.clear();
        // Enhanced rings: also always produced (tap_channels is on), so the
        // enhanced path stays warm for a live switch.
        for pair in bus.psg_tap.chunks_exact(2) {
            if st.psg.len() < 65536 {
                st.psg.push_back(pair[0] as f32 / 32768.0);
                st.psg.push_back(pair[1] as f32 / 32768.0);
            } else {
                st.drops += 1;
            }
        }
        bus.psg_tap.clear();
        // MP2K/engine HLE stream and liveness. A pause is a diagnostic event
        // (the crossfade keeps it inaudible) — say so, with the reason, but
        // don't let a hopeless variant spam the log forever.
        let live = bus.mp2k.as_deref().is_some_and(|h| h.live())
            || bus.gax.as_deref().is_some_and(|g| g.live())
            || bus.rdrv.as_deref().is_some_and(|r| r.live());
        let pause_msg = if let Some(h) = bus.mp2k.as_deref_mut() {
            h.vf.reverted.take().map(|m| (m, h.vf.pauses))
        } else if let Some(g) = bus.gax.as_deref_mut() {
            g.vf.reverted.take().map(|m| (m, g.vf.pauses))
        } else if let Some(r) = bus.rdrv.as_deref_mut() {
            r.vf.reverted.take().map(|m| (m, r.vf.pauses))
        } else {
            None
        };
        if let Some((msg, pauses)) = pause_msg {
            if pauses <= 3 {
                eprintln!("DEGRADED: engine HLE paused (probing continues): {msg}");
            } else if pauses == 4 {
                eprintln!("DEGRADED: engine HLE paused again: {msg} (further pauses unlogged)");
            }
        }
        // Feed the HLE ring only while live — an unproven stream must neither
        // pile up (drop spam) nor leave stale audio for the switchover.
        if live {
            for pair in bus.hle_tap.chunks_exact(2) {
                if st.hle.len() < 65536 {
                    st.hle.push_back(pair[0]);
                    st.hle.push_back(pair[1]);
                } else {
                    st.drops += 1;
                }
            }
        } else {
            st.hle.clear();
        }
        bus.hle_tap.clear();
        // Switch over only once the HLE ring is primed; the callback
        // crossfades both directions, and the per-channel path stays warm.
        if !live {
            st.hle_on = false;
        } else if st.hle.len() / 2 >= RING_TARGET / 2 {
            st.hle_on = true;
        }
        for f in 0..2 {
            let events = std::mem::take(&mut bus.fifo_tap[f]);
            for e in events {
                if st.fifo[f].len() < 32768 {
                    st.fifo[f].push_back(e);
                } else {
                    st.drops += 1;
                }
            }
        }
    }

    /// The audio-device watchdog (runs on the outer tick, not the production
    /// loop — a dead consumer leaves the ring full and the production loop
    /// never executes, which is exactly the case this must catch).
    fn audio_watchdog(&mut self) {
        if self.stream.is_some() && self.audio_watch.elapsed().as_secs() >= 1 {
            self.audio_watch = Instant::now();
            let (consumed, dead) = {
                let st = self.streams.lock().unwrap();
                (st.consumed, st.dead)
            };
            if dead || consumed == self.audio_consumed_last {
                self.audio_stalls += 1;
            } else {
                self.audio_stalls = 0;
            }
            self.audio_consumed_last = consumed;
            if dead || self.audio_stalls >= 2 {
                eprintln!(
                    "DEGRADED: audio device {} — wall-clock pacing, retrying every 5 s",
                    if dead { "errored" } else { "stopped consuming" }
                );
                self.stream = None;
                {
                    let mut st = self.streams.lock().unwrap();
                    st.dead = false;
                    st.mixed.clear();
                    st.psg.clear();
                    st.hle.clear();
                    st.fifo[0].clear();
                    st.fifo[1].clear();
                }
                self.audio_stalls = 0;
                self.audio_retry = Some(Instant::now());
                self.next_frame = Instant::now();
            }
        } else if self.stream.is_none() {
            if let Some(t) = self.audio_retry {
                if t.elapsed().as_secs() >= 5 {
                    self.audio_retry = Some(Instant::now());
                    if let Some(s) = start_audio(self.streams.clone()) {
                        eprintln!("audio: output device restored");
                        self.stream = Some(s);
                        self.primed = false;
                        self.audio_consumed_last = 0;
                        self.audio_stalls = 0;
                        reset_audio_counters(&self.streams);
                    }
                }
            }
        }
    }
}

impl ApplicationHandler for PlayApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create the window + presenter exactly once. `resumed` can fire again
        // on some platforms (e.g. Android lifecycle); on desktop, once.
        if self.window.is_some() {
            return;
        }
        // Self-paced: poll continuously and produce frames as the audio ring
        // needs them. Never Wait (would block the emulator on an OS event).
        event_loop.set_control_flow(ControlFlow::Poll);
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_inner_size(LogicalSize::new(240.0 * 3.0, 160.0 * 3.0))
            .with_resizable(true);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(format!("window: {e}"));
                event_loop.exit();
                return;
            }
        };
        self.presenter = match screen::present::Presenter::new(
            window.clone(),
            240,
            160,
            self.display_target,
            self.av.video_vsync,
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("video: GPU presenter unavailable ({e}); basic scaling active");
                None
            }
        };
        self.window = Some(window);
        // The window now exists: tell a supervising frontend we're live.
        if self.status {
            status_line("playing");
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // Lost focus: a key held during alt-tab might never deliver its
            // release, so drop the held set defensively (no stuck inputs).
            WindowEvent::Focused(false) => self.held.clear(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        repeat,
                        ..
                    },
                ..
            } => match state {
                ElementState::Pressed => {
                    self.held.insert(code);
                    // F11 toggles fullscreen (the OS's own fullscreen
                    // affordance — macOS green button, Linux/Windows compositor
                    // — also works under winit). Act on the initial press, not
                    // auto-repeat.
                    if code == KeyCode::F11 && !repeat {
                        if let Some(w) = self.window.as_ref() {
                            let next = if w.fullscreen().is_some() {
                                None
                            } else {
                                Some(Fullscreen::Borderless(None))
                            };
                            w.set_fullscreen(next);
                        }
                    }
                }
                ElementState::Released => {
                    self.held.remove(&code);
                }
            },
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            return;
        }

        // Pointer policy, reconciled live: visible in windowed mode (users
        // need it to drag/resize/close the window) and while the pause menu is
        // up; hidden only when the game owns a fullscreen screen. Call through
        // only on a change.
        let fullscreen = self
            .window
            .as_ref()
            .is_some_and(|w| w.fullscreen().is_some());
        let want_cursor = self.paused || !fullscreen;
        if want_cursor != self.cursor_visible {
            if let Some(w) = self.window.as_ref() {
                w.set_cursor_visible(want_cursor);
            }
            self.cursor_visible = want_cursor;
        }

        let (mut keys, nav) = self.gather_input();

        // Drop any button from the suppression mask once observed released
        // (active-low bit set → cleared from `!keys`), then force the buttons
        // still held over from a menu interaction to read as released.
        self.suppress &= !keys;
        keys |= self.suppress;

        // The open control (Escape, or the pad's guide button) toggles the
        // pause overlay. With the menu disabled (a package pin), its rising
        // edge quits instead, preserving the historical Escape = quit.
        let toggle_edge = nav.open && !self.nav_prev.open;
        if toggle_edge {
            if !self.menu_enabled {
                event_loop.exit();
                return;
            }
            self.paused = !self.paused;
            if self.paused {
                self.menu.reset();
            }
            reset_audio_counters(&self.streams);
            // A toggle press is fully consumed here — while paused, fall
            // through to the overlay redraw; the same press must not also be
            // fed to the menu as a Cancel.
            if !self.paused {
                self.suppress = !keys & 0x3FF;
                self.nav_prev = nav;
                return;
            }
        }

        if self.paused {
            // Feed the menu the rising edges and apply any setting change live
            // (video) or to av.cfg only (restart-required audio/gamut). The
            // GBA sees all keys released while the menu is up.
            self.machine.bus.keys = 0x3FF;
            let events = [
                (nav.up && !self.nav_prev.up, menu::MenuInput::Up),
                (nav.down && !self.nav_prev.down, menu::MenuInput::Down),
                (nav.left && !self.nav_prev.left, menu::MenuInput::Left),
                (nav.right && !self.nav_prev.right, menu::MenuInput::Right),
                (
                    nav.confirm && !self.nav_prev.confirm,
                    menu::MenuInput::Confirm,
                ),
                // East backs out; Escape already toggled above, so only feed
                // Cancel when this frame wasn't a toggle.
                (
                    !toggle_edge && nav.cancel && !self.nav_prev.cancel,
                    menu::MenuInput::Cancel,
                ),
            ];
            self.nav_prev = nav;
            let mut quit = false;
            for (fired, ev) in events {
                if !fired {
                    continue;
                }
                if let Some(action) = self.menu.handle(ev, &mut self.av) {
                    match action {
                        menu::MenuAction::Resume => self.paused = false,
                        menu::MenuAction::Quit => quit = true,
                        menu::MenuAction::Changed(what) => {
                            // Vsync is the presenter's own knob; everything
                            // else routes through apply_av_change.
                            if what == menu::Changed::Vsync {
                                if let Some(p) = self.presenter.as_mut() {
                                    p.set_vsync(self.av.video_vsync);
                                }
                            }
                            apply_av_change(
                                what,
                                &self.av,
                                &self.streams,
                                &mut self.screen_kind,
                                &mut self.response,
                                &mut self.lut,
                                &mut self.temporal,
                                &mut self.grid_params,
                                self.lut_primaries,
                            );
                            if let Err(e) = self.av.save() {
                                eprintln!("menu: could not save av.cfg ({e})");
                            }
                        }
                    }
                }
            }
            if quit {
                event_loop.exit();
                return;
            }
            if !self.paused {
                // Resume: clear the counters again so the pause tail doesn't
                // bleed into the next report, and neutralize the buttons held
                // to select Resume/back out (South=A, East=B) so they don't
                // buffer into the game. The cursor is re-hidden by the next
                // reconcile if fullscreen.
                reset_audio_counters(&self.streams);
                self.suppress = !keys & 0x3FF;
                return;
            }
            // Redraw the overlay over the last emulated frame at a modest fixed
            // cadence. Re-compose from the frozen `temporal` each time
            // (emulation, temporal, and the frame counter all stand still while
            // paused) so the overlay's darkening never compounds across
            // redraws.
            self.temporal.compose(&self.lut, &mut self.rgba);
            self.menu.draw(&self.av, &mut self.rgba, 240, 160);
            self.present_rgba();
            std::thread::sleep(Duration::from_millis(16));
            return;
        }
        self.nav_prev = nav;

        self.machine.bus.keys = keys;
        if let Some((f, last)) = &mut self.record {
            if keys != *last {
                use std::io::Write;
                let _ = writeln!(f, "{} {keys:03x}", self.machine.bus.frames);
                *last = keys;
            }
        }

        // Audio-clock pacing: the device callback consumes at a fixed rate, so
        // run exactly as many frames as the ring needs — production follows
        // consumption, immune to display refresh rate (ProMotion) and window-
        // server throttling. Without a device, wall-clock pace at the hardware
        // frame rate (handled below).
        let mut ran = 0u32;
        while ran < 4 {
            if self.stream.is_some() {
                // Both rings are live (the menu can crossfade between them), so
                // pace on the emptier one — neither faithful nor enhanced may
                // be allowed to starve.
                let fill = {
                    let st = self.streams.lock().unwrap();
                    (st.mixed.len() / 2).min(st.psg.len() / 2)
                };
                if fill >= RING_TARGET {
                    break;
                }
            } else if ran >= 1 {
                break;
            }
            ran += 1;
            self.run_one_frame();
        }

        // Once both rings first reach target, the buffer has primed: discard
        // the cold-start underruns so they don't report as a defect.
        if !self.primed && self.stream.is_some() {
            let st = self.streams.lock().unwrap();
            if (st.mixed.len() / 2).min(st.psg.len() / 2) >= RING_TARGET {
                drop(st);
                self.primed = true;
                reset_audio_counters(&self.streams);
            }
        }

        self.audio_watchdog();

        if ran == 0 {
            // Ring is full: let the device drain a little before the next tick.
            // (winit pumps OS events between ticks; no explicit pump needed.)
            std::thread::sleep(Duration::from_millis(2));
            return;
        }
        if self.stream.is_none() {
            self.next_frame += Duration::from_nanos(16_742_706); // 59.7275 Hz
            let now = Instant::now();
            if self.next_frame > now {
                std::thread::sleep(self.next_frame - now);
            } else {
                self.next_frame = now;
            }
        }

        // Resolve the visible image (temporal response + color LUT) and
        // present it (downstream of production, never the pacing source).
        self.temporal.compose(&self.lut, &mut self.rgba);
        self.present_rgba();
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Final teardown on a clean exit (window closed / menu Quit): this is
        // winit's guaranteed last callback, so it runs whether or not
        // `run_app` returns on a given platform. The in-loop periodic save
        // bounds loss on an abnormal (signal) kill. Skip if we're already
        // failing (e.g. window creation never succeeded).
        if self.error.is_some() {
            return;
        }
        if let Some(data) = self.machine.bus.save_data() {
            match write_sav(&self.sav_path, data) {
                Ok(()) => eprintln!("saved {}", self.sav_path),
                Err(e) => {
                    self.error = Some(format!("{}: {e}", self.sav_path));
                    return;
                }
            }
        }
        print_fallback_census();
        if FALLBACK_COLLECT.load(Ordering::Relaxed) {
            if let Err(e) = record_labels(
                self.machine.bus.rom.len(),
                &rom_sha256(&self.machine.bus.rom),
            ) {
                self.error = Some(e);
            }
        }
    }
}

/// Interactive play: resolve the image/BIOS/translation, set up audio, input,
/// and screen simulation, then run the winit event loop until the window
/// closes. Teardown (final save, fallback census, label recording) happens
/// after the loop returns.
pub fn cmd_play(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut interp_only = false;
    // Machine-readable lifecycle lines on stdout for a supervising frontend
    // (the launcher): `STATUS building <pct>` / `STATUS playing`.
    let mut status = false;
    // Perf instrumentation is developer tooling: always on in debug builds,
    // opt-in (--stats or GBA_RECOMP_STATS=1) in release.
    let mut show_stats = cfg!(debug_assertions) || std::env::var_os("GBA_RECOMP_STATS").is_some();
    let mut bios_arg: Option<String> = None;
    let mut no_bios = false;
    let mut gamedb_arg: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--interp" => interp_only = true,
            "--status" => status = true,
            "--stats" => show_stats = true,
            "--record-labels" => FALLBACK_COLLECT.store(true, Ordering::Relaxed),
            "--bios" => bios_arg = Some(it.next().ok_or("--bios needs a value")?.to_string()),
            "--no-bios" => no_bios = true,
            "--gamedb" => gamedb_arg = Some(it.next().ok_or("--gamedb needs a value")?.to_string()),
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    // The function-boundary source for a first-launch build: open it once
    // here so on-demand translation seeds exhaustively from the map (and
    // --ram profiling captures RAM-resident code the map can't carry).
    let gamedb = gamedb_arg
        .as_deref()
        .map(|p| gamedb::GameDb::open(std::path::Path::new(p)))
        .transpose()?;
    // Packaged mode: a manifest beside the executable pins this binary to one
    // title. The ROM is resolved by content hash (the package ships none), the
    // BIOS pin is enforced, and the translation loads from the package.
    let manifest = match crate::packfile::load() {
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    let rom_path = match (&rom_path, &manifest) {
        (Some(p), _) => p.clone(),
        (None, Some(m)) => {
            let p = crate::packfile::find_rom(m)?;
            eprintln!("{}: playing {}", m.name, p.display());
            p.to_string_lossy().into_owned()
        }
        (None, None) => return Err("missing ROM path".into()),
    };
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    validate_rom(&rom_path, &rom)?;
    if let Some(m) = &manifest {
        let sha = rom_sha256(&rom);
        if sha != m.rom_sha256 {
            return Err(format!(
                "this package plays exactly one cartridge image \
(sha256 {}…); {rom_path} hashes to {}…",
                &m.rom_sha256[..16],
                &sha[..16]
            ));
        }
    }
    let sav_path = format!("{}.sav", rom_path.trim_end_matches(".gba"));

    // Product boot path: real BIOS when an image is installed (explicit --bios
    // > the launcher-installed image via the shared resolution), BIOS HLE
    // otherwise. An explicit --bios that fails to load is a hard error; a
    // discovered image that fails is loud but non-fatal.
    let bios: Option<Vec<u8>> = if no_bios {
        eprintln!("boot: BIOS HLE (--no-bios)");
        None
    } else if let Some(p) = &bios_arg {
        let b = load_bios_file(p)?;
        eprintln!("boot: real BIOS ({p})");
        Some(b)
    } else if let Some(p) = input_config::find_bios() {
        match load_bios_file(&p) {
            Ok(b) => {
                eprintln!("boot: real BIOS ({})", p.display());
                Some(b)
            }
            Err(e) => {
                eprintln!("DEGRADED: installed BIOS unusable ({e}); using BIOS HLE");
                None
            }
        }
    } else {
        eprintln!("boot: BIOS HLE (no BIOS image installed)");
        None
    };
    if let Some(b) = &bios {
        use sha2::{Digest, Sha256};
        let sha = Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>();
        if let Some(m) = &manifest {
            if sha != m.bios_sha256 {
                return Err(format!(
                    "this package requires the pinned BIOS (sha256 {}…); the installed \
image hashes to {}…",
                    &m.bios_sha256[..16],
                    &sha[..16]
                ));
            }
        } else if sha != input_config::BIOS_SHA256 {
            eprintln!("bios: image is not the canonical dump (sha256 {sha}) — trying it as-is");
        }
    } else if let Some(m) = &manifest {
        return Err(format!(
            "this package requires a BIOS image (sha256 {}…) and none is installed.\n\
Place your own dump as gba_bios.bin next to the executable:\n  {}",
            &m.bios_sha256[..16],
            m.dir.display()
        ));
    }

    // Native translation. Packaged: the prebuilt library ships in the package
    // — under interpreter=false (a *full recomp*) failing to load it is fatal,
    // and so is any dispatch miss later. Developer path: per-user cache, built
    // on first launch. The product bar is full speed at full accuracy.
    let native = if let Some(m) = &manifest {
        if interp_only && !m.interpreter {
            return Err("--interp is not available in a full-recomp package".into());
        }
        if !m.interpreter {
            NO_INTERP.store(true, Ordering::Relaxed);
        }
        let lib_path = m.dir.join(&m.translation);
        match load_native(&lib_path).inspect(|v| {
            eprintln!("native translation: {} blocks (packaged)", v.1.len);
        }) {
            Ok(v) if !interp_only => Some(v),
            Ok(_) => None,
            Err(e) if !m.interpreter => return Err(e),
            Err(e) => {
                eprintln!("DEGRADED: packaged translation unavailable ({e}); interpreter only");
                None
            }
        }
    } else if interp_only {
        None
    } else {
        match ensure_native(&rom_path, &rom, status, bios.as_deref(), gamedb.as_ref()) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("DEGRADED: native translation unavailable ({e}); interpreter only");
                None
            }
        }
    };

    let mut machine = make_machine(rom, bios.as_deref());
    if let Ok(sav) = std::fs::read(&sav_path) {
        machine.bus.load_save_data(&sav);
        eprintln!("loaded {sav_path}");
    }

    // Triage (RECOMP_RECORD_INPUT=<path>): record this session's input as a
    // replayable script (frames/runc/verify --input). The save state at
    // session start is snapshotted next to it so the replay boots from
    // identical backup media.
    let record: Option<(std::fs::File, u16)> = match std::env::var_os("RECOMP_RECORD_INPUT") {
        None => None,
        Some(p) => {
            use std::io::Write;
            let p = p.to_string_lossy().into_owned();
            if Path::new(&sav_path).is_file() {
                let _ = std::fs::copy(&sav_path, format!("{p}.sav"));
            }
            let mut f = std::fs::File::create(&p).map_err(|e| format!("{p}: {e}"))?;
            writeln!(f, "gba-input v1").map_err(|e| e.to_string())?;
            eprintln!("recording input to {p}");
            Some((f, 0x3FF))
        }
    };

    let title = Path::new(&rom_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recomp")
        .to_string();

    // Audio. Both paths run at once so the in-game menu can crossfade between
    // faithful and enhanced live (the callback slews between them, the producer
    // fills both rings). tap_channels gates only the extra per-channel work;
    // the faithful mix is produced regardless. The MP2K/engine HLE shadow is
    // armed when the stock driver is present so the enhanced path is ready the
    // instant it's selected; it's read-only, so arming it always is free of
    // correctness risk.
    let av = input_config::AvConfig::load();
    machine.bus.tap_channels = true;
    let desc = arm_audio_hle(
        &mut machine,
        manifest.as_ref().and_then(|m| m.engine_hle.as_deref()),
    );
    eprintln!("audio engine: {desc}");
    let streams = Arc::new(Mutex::new(AudioStreams::default()));
    streams.lock().unwrap().enhanced_on = av.audio_enhanced;
    let stream = start_audio(streams.clone());

    // Bindings come from the launcher-managed config (defaults match the
    // historical hardcoded map). Keyboard always works; a configured gamepad is
    // OR-ed in on top.
    let icfg = input_config::InputConfig::load();
    let defaults = input_config::InputConfig::default();
    let key_pairs: Vec<(KeyCode, u16)> = input_config::Button::ALL
        .iter()
        .map(|b| {
            let name = &icfg.keys[b.index()];
            let key = winit_key(name).unwrap_or_else(|| {
                eprintln!(
                    "input.cfg: unknown key {name:?} for {} — using default",
                    b.name()
                );
                winit_key(&defaults.keys[b.index()]).unwrap()
            });
            (key, b.bit())
        })
        .collect();
    let pad = match icfg.device {
        input_config::Device::Gamepad => build_gilrs(),
        input_config::Device::Keyboard => None,
    };

    // Screen simulation (see crates/screen): temporal response on the emulated-
    // frame stream, color LUT to the display's colorspace, and a GPU grid pass.
    // Settings come from the launcher-managed A/V config; unknown values fall
    // back loudly. `av` is mutable: the in-game menu edits it live, so the
    // screen objects derived from it rebuild in place on a change.
    let screen_kind = screen::ScreenKind::by_name(&av.screen).unwrap_or_else(|| {
        eprintln!(
            "av.cfg: unknown video.screen {:?} — using frontlit",
            av.screen
        );
        screen::ScreenKind::Frontlit
    });
    let display_target = screen::DisplayTarget::by_name(&av.display_gamut).unwrap_or_else(|| {
        eprintln!(
            "av.cfg: unknown video.gamut {:?} — using auto",
            av.display_gamut
        );
        screen::DisplayTarget::Auto
    });
    // Resolve the LUT's target primaries from the configured gamut plus OS
    // detection (a wide-gamut panel the compositor isn't managing resolves to
    // the panel's own primaries — see screen::display). EDID is sysfs and the
    // probe opens its own Wayland connection, so this needs no window.
    let lut_primaries = screen::display::resolve(display_target);
    let lut = screen::ColorLut::build(&screen::ColorSettings {
        screen: screen_kind,
        darken: input_config::AvConfig::knob(&av.screen_darken)
            .map(f64::from)
            .unwrap_or(f64::NAN),
        target: lut_primaries,
    });
    let response = screen::ResponseMode::by_name(&av.response).unwrap_or_else(|| {
        eprintln!(
            "av.cfg: unknown video.response {:?} — using smart",
            av.response
        );
        screen::ResponseMode::Smart
    });
    let temporal = screen::Temporal::new(
        240 * 160,
        response,
        0,
        input_config::AvConfig::knob(&av.response_keep)
            .unwrap_or_else(|| screen::blend::default_rho(screen_kind)),
    );
    let grid_params = screen::present::GridParams::with_strength(
        input_config::AvConfig::knob(&av.grid)
            .unwrap_or_else(|| screen_kind.default_grid_strength()),
    );

    // In-game menu (Escape / pad Mode). Available unless a package pins it off,
    // in which case Escape quits as it did before the menu existed.
    let menu_enabled = manifest.as_ref().map(|m| m.menu).unwrap_or(true);

    let sav_seen = machine.bus.save_data().map(fnv64);
    let mut app = PlayApp {
        machine,
        sav_path,
        sav_seen,
        record,
        native,
        streams,
        stream,
        icfg,
        key_pairs,
        pad,
        held: HashSet::new(),
        av,
        screen_kind,
        display_target,
        lut_primaries,
        lut,
        response,
        temporal,
        grid_params,
        rgba: vec![[0u8; 4]; 240 * 160],
        title,
        status,
        window: None,
        presenter: None,
        sb_context: None,
        sb_surface: None,
        menu_enabled,
        menu: menu::Menu::new(true, false),
        paused: false,
        // Starts visible (windowed); the loop reconciles against fullscreen.
        cursor_visible: true,
        suppress: 0,
        nav_prev: NavEdges::default(),
        show_stats,
        emu_ema_ms: 0.0,
        frames_run: 0,
        slow_warned: false,
        native_run: 0,
        fallback_run: 0,
        primed: false,
        next_frame: Instant::now(),
        audio_watch: Instant::now(),
        audio_consumed_last: 0,
        audio_stalls: 0,
        audio_retry: None,
        error: None,
    };

    // Teardown (final save, fallback census, label recording) runs in the
    // app's `exiting` callback — winit's guaranteed last hook — so it fires on
    // a clean window close regardless of whether `run_app` returns. Anything
    // it raised surfaces through `app.error`.
    let event_loop = EventLoop::new().map_err(|e| format!("event loop: {e}"))?;
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("event loop: {e}"))?;
    if let Some(e) = app.error {
        return Err(e);
    }
    Ok(())
}
