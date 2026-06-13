//! In-house (R) sound-driver HLE shadow mixer (the engine that ships
//! its "AUDIO ERROR" diagnostics — see `engine::Engine::Rdiag`).
//!
//! The driver is a MONO software mixer: once per frame its tick runs
//! sequencer → ADSR → **mix** (224 samples @ 13379 Hz of s8 voices into
//! an s16 IWRAM accumulator, saturated to s8 into a double buffer) →
//! **advance** (positions committed in place in the voice structs).
//! Every VBlank the front buffer is re-armed on DMA1 → FIFO A (the only
//! FIFO used; routed to both speakers).
//!
//! The whole driver core is one ARM blob copied raw to IWRAM at init,
//! which makes the shadow surface ideal: a byte-signature scan finds the
//! blob, the mix/advance entries are found by opcode-shape signatures,
//! and every global the mixer touches is harvested from the blob's own
//! pc-relative literals — no per-title constants anywhere.
//!
//! Resync model (GAX-style, zero drift): the mix-entry hook snapshots
//! voice state while it is final-for-the-chunk and untouched; the
//! advance-entry hooks then pick up each voice's **step cache** (+0x08,
//! written by the guest mixer with the exact 9.23 step it used, consumed
//! and zeroed by the guest advance pass right after our hook). A nonzero
//! cache also marks exactly the voices the guest rendered, so the
//! 8-voice mix budget and revision-specific voice-stealing come along
//! for free. The guest owns all struct updates — the shadow only reads.

use crate::mp2k::MemView;
use crate::shadow::Verifier;

/// ARM prologue of the blob's note-on routine — the blob base. Present
/// in ROM (copy source) and IWRAM (live) in every known build.
const NOTE_ON_SIG: [u8; 16] = [
    0x00, 0x40, 0x2D, 0xE9, 0x03, 0x30, 0xC5, 0xE5, 0x24, 0x10, 0x85, 0xE5, 0x01, 0x70, 0xC5, 0xE5,
];

/// Voice struct layout (stride 0x28), identical across the family.
const V_STATE: u32 = 0x00;
const V_MODE: u32 = 0x01;
const V_VOL: u32 = 0x03;
const V_GRP: u32 = 0x06;
const V_STEP: u32 = 0x08;
const V_POS: u32 = 0x0C;
const V_FRAC: u32 = 0x10;
const V_ENV: u32 = 0x14;
const V_INSTR: u32 = 0x20;
const V_STRIDE: u32 = 0x28;
/// Instrument header fields the mixer path reads.
const I_LOOP_LEN: u32 = 0x14;
const I_END: u32 = 0x18;

const MAX_VOICES: usize = 128;

/// Detection result: blob source located in ROM (the IWRAM copy is
/// found again at runtime — same bytes, possibly any base).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdrvSig {
    pub blob_rom_off: usize,
}

pub fn detect(rom: &[u8]) -> Option<RdrvSig> {
    crate::engine::find(rom, &NOTE_ON_SIG, 0).map(|pos| RdrvSig { blob_rom_off: pos })
}

/// One shadowed voice. `pos` is an absolute guest byte address (s8 PCM,
/// usually ROM) plus the 23-bit fraction; `step` is the guest's own
/// effective step for this chunk (from the +0x08 cache).
#[derive(Clone, Copy)]
struct Voice {
    on: bool,
    /// Struct address, for the step pickup at the advance hook.
    addr: u32,
    pos: f64,
    end: f64,
    loop_len: u32,
    /// One-shot (1/3) vs loop (2/4) per the sample mode byte.
    looping: bool,
    /// Source samples per guest output sample.
    step: f64,
    /// Guest position as snapshotted at the mix hook (render mutates
    /// `pos`; this keeps the tick's ground truth for telemetry).
    guest_pos: f64,
    /// Wave identity for telemetry validity (same note, no restart).
    instr: u32,
    /// Source samples per 65536 Hz grid sample. NaN until derived from
    /// guest position telemetry or the step-cache fallback.
    grid_step: f64,
    /// Integer gain chain result (0..~0x80) over 128.
    gain: f32,
    /// Render-set ramp target: this window ends at this gain (the next
    /// tick's gain, or 0 for a voice's final window).
    gain_target: f32,
    dead: bool,
}

impl Default for Voice {
    fn default() -> Voice {
        Voice {
            on: false,
            addr: 0,
            pos: 0.0,
            end: 0.0,
            loop_len: 0,
            looping: false,
            step: 0.0,
            guest_pos: 0.0,
            instr: 0,
            grid_step: f64::NAN,
            gain: 0.0,
            gain_target: 0.0,
            dead: false,
        }
    }
}

/// Globals harvested from the blob's literals at engage time.
#[derive(Default, Clone, Copy)]
struct Layout {
    /// Hook PCs (ARM, IWRAM): mix entry, advance_music, advance_fx.
    mix_all: u32,
    adv_music: u32,
    adv_fx: u32,
    /// Voice arrays: base + count (counts deref'd from ROM words).
    fx_base: u32,
    fx_count: u32,
    music_base: u32,
    music_count: u32,
    /// Music-mute gate (u32; ==1 skips the music array entirely).
    gate: u32,
    /// Chunk sample count global (0xE0 on the shipped rate).
    chunk_ptr: u32,
    /// Gain globals: FX master, tune master, channel volume table, and
    /// the later revision's extra per-channel table (0 = absent).
    fxvol: u32,
    tunevol: u32,
    chanvol: u32,
    chanvol2: u32,
}

pub struct RdrvHle {
    pub active: bool,
    pub engaged: bool,
    pub vf: Verifier,
    pub hooks: u64,
    pub stale_ticks: u64,
    pub bad_waves: u64,
    lay: Layout,
    /// Snapshot taken at the mix hook; activated by the step pickup.
    pending: [Voice; MAX_VOICES],
    voices: [Voice; MAX_VOICES],
    count: usize,
    /// Grid samples per guest output sample (65536 / mix rate).
    mix_step: f64,
    /// Stream-level canon-domain ZOH (mono).
    chk_hold: f32,
    chk_acc: f64,
    engage_backoff: u32,
    last_hook_cursor: u64,
    /// Cursor of the last step pickup, so the fx-advance hook only
    /// commits when the music pass didn't (music paused).
    steps_cursor: u64,
    /// Render-window gain ramp: samples into the window / expected
    /// window length (the last hook gap).
    win_pos: f64,
    win_len: f64,
    /// Tick-cadence statistics: long-run MEAN of the hook gap (the tick
    /// is frame-locked underneath; IRQ-latency jitter cancels in the
    /// mean), with a 16-hook recent window that re-seeds it on a real
    /// cadence change. Same scheme as the GAX audit's mix-rate fix —
    /// an EMA's short memory turned hook jitter into per-tick pitch
    /// wobble in the telemetry denominator.
    cad_recent: [f64; 16],
    cad_idx: u32,
    cad_sum: f64,
    cad_n: u32,
    /// A mix hook has fired since (re-)engage: until then the
    /// starvation watchdog must not run (a fresh engage has no cursor
    /// baseline — disengaging on it flaps at probe cadence).
    hook_seen: bool,
    /// Stale-chunk replay: the guest's FIFO DMA cycles the two-chunk
    /// output ring, so when the game stops calling the mixer (observed:
    /// a 1.4 s outage right after a note-on — the DMA loops that bass
    /// chunk as a tone) the hardware-faithful output is the LAST TWO
    /// COMMITTED CHUNKS looping, not silence. We keep the check
    /// stream's most recent 2×chunk guest-rate samples and loop them
    /// whenever hooks stop; output, check copy, and verifier all see
    /// the replay, so outages stay proven and level-matched.
    replay: [f32; 1024],
    replay_cap: usize,
    replay_w: usize,
    replay_r: usize,
    replaying: bool,
    /// Discontinuity attribution (RECOMP_RDRV_DISC): tally output-step
    /// magnitude by cause — boundary (window rebuild), loop-wrap,
    /// voice-death, interior. Off the hot path unless armed.
    dbg_disc: bool,
    dbg_prev_out: f32,
    dbg_new_window: bool,
    dbg: [(u64, f64); 4], // (count, sum|Δ|) for [boundary, wrap, death, interior]
    dbg_err_n: u64,
    dbg_err_sum: f64,
    dbg_err_signed: f64,
    dbg_snap_n: u64,
}

impl RdrvHle {
    pub fn new(_sig: RdrvSig) -> RdrvHle {
        RdrvHle {
            active: true,
            engaged: false,
            vf: Verifier::new(),
            hooks: 0,
            stale_ticks: 0,
            bad_waves: 0,
            lay: Layout::default(),
            pending: [Voice::default(); MAX_VOICES],
            voices: [Voice::default(); MAX_VOICES],
            count: 0,
            mix_step: 65536.0 / 13379.0,
            chk_hold: 0.0,
            chk_acc: 0.0,
            engage_backoff: 0,
            last_hook_cursor: 0,
            steps_cursor: 0,
            win_pos: 0.0,
            win_len: f64::MAX,
            cad_recent: [0.0; 16],
            cad_idx: 0,
            cad_sum: 0.0,
            cad_n: 0,
            hook_seen: false,
            replay: [0.0; 1024],
            replay_cap: 0,
            replay_w: 0,
            replay_r: 0,
            replaying: false,
            dbg_disc: std::env::var_os("RECOMP_RDRV_DISC").is_some(),
            dbg_prev_out: 0.0,
            dbg_new_window: false,
            dbg: [(0, 0.0); 4],
            dbg_err_n: 0,
            dbg_err_sum: 0.0,
            dbg_err_signed: 0.0,
            dbg_snap_n: 0,
        }
    }

    /// Drain the discontinuity-attribution tallies (RECOMP_RDRV_DISC).
    pub fn disc_report(&self) -> String {
        let lab = ["boundary", "wrap", "death", "interior"];
        let mut s = String::from("rdrv disc |Δ| by cause:");
        for (i, (n, sum)) in self.dbg.iter().enumerate() {
            let mean = if *n > 0 { sum / *n as f64 } else { 0.0 };
            s += &format!(" {}={:.5}(n={})", lab[i], mean, n);
        }
        let mean_err = if self.dbg_err_n > 0 {
            self.dbg_err_sum / self.dbg_err_n as f64
        } else {
            0.0
        };
        let bias = if self.dbg_err_n > 0 {
            self.dbg_err_signed / self.dbg_err_n as f64
        } else {
            0.0
        };
        s += &format!(
            "; carry mean|err|={mean_err:.3} bias={bias:+.3} (carry={} snap={})",
            self.dbg_err_n, self.dbg_snap_n
        );
        s
    }

    #[inline(always)]
    pub fn hook_match(&self, key: u32) -> bool {
        self.engaged
            && (key == self.lay.mix_all || key == self.lay.adv_music || key == self.lay.adv_fx)
    }

    /// Periodic pre-engage probe (driven from the audio pump): find the
    /// live IWRAM copy of the driver blob and harvest every global from
    /// its own code. Cheap: a byte scan a few times per second until the
    /// guest's init has run.
    pub fn try_engage(&mut self, mem: &MemView) {
        if self.engaged || !self.active {
            return;
        }
        self.engage_backoff = self.engage_backoff.wrapping_add(1);
        if self.engage_backoff % 16 != 0 {
            return;
        }
        let Some(off) = crate::engine::find(mem.iwram, &NOTE_ON_SIG, 0) else {
            return;
        };
        let blob = 0x0300_0000 + off as u32;
        if let Some(lay) = harvest(mem, blob) {
            self.lay = lay;
            self.engaged = true;
            self.hook_seen = false;
            if std::env::var_os("RECOMP_RDRV_TRACE").is_some() {
                eprintln!("rdrv: re-engage hooks={}", self.hooks);
            }
        }
    }

    /// Mix-entry hook: every voice's state for the upcoming chunk is
    /// final (sequencer + envelopes already ran; positions are
    /// chunk-start; the guest's advance pass commits them after mixing).
    fn mix_hook(&mut self, mem: &MemView, audio_cursor: u64) {
        // Re-fired inside the same grid sample (fallback double
        // dispatch): idempotent by construction — skip entirely so the
        // diagnostics and cadence stay sane.
        let gap =
            audio_cursor.saturating_sub(self.last_hook_cursor) / crate::mem::AUDIO_SAMPLE_CYCLES;
        if gap < 8 && self.hooks > 0 {
            return;
        }
        self.hooks += 1;
        // Liveness: the blob is copied once and stays put, but the hook
        // word is one load away — verify and rescan on any rebuild.
        if mem.u32(self.lay.mix_all).map(|w| w & 0xFFFF_F000) != Some(0xE24D_D000) {
            self.stale_ticks += 1;
            self.engaged = false;
            return;
        }
        // Hook cadence is the tick rate; chunk count makes it the mix
        // rate (self-calibrating; the engine has a 4-row rate table).
        // The cadence is kept as a long-run MEAN, not an EMA: the tick
        // is frame-locked underneath, so hook-observation jitter
        // (sequencer load ahead of the mix entry) cancels in the mean,
        // while an EMA fed it straight into mix_step and the telemetry
        // denominator as per-tick pitch wobble. A 16-hook recent
        // window re-seeds the mean if the cadence genuinely moves.
        let chunk = self.lay.chunk_ptr_deref(mem).unwrap_or(224).clamp(64, 512) as f64;
        self.last_hook_cursor = audio_cursor;
        self.hook_seen = true;
        if (256..=4096).contains(&gap) {
            let g = gap as f64;
            self.cad_recent[(self.cad_idx % 16) as usize] = g;
            self.cad_idx += 1;
            self.cad_sum += g;
            self.cad_n += 1;
            if self.cad_n > 4096 {
                self.cad_sum *= 0.5;
                self.cad_n /= 2;
            }
            if self.cad_idx >= 16 {
                let recent = self.cad_recent.iter().sum::<f64>() / 16.0;
                let mean = self.cad_sum / self.cad_n as f64;
                if (recent / mean - 1.0).abs() > 0.05 {
                    // Real cadence change: restart from the recent window.
                    self.cad_sum = recent * 16.0;
                    self.cad_n = 16;
                }
            }
            self.mix_step = if self.cad_n >= 8 {
                self.cad_sum / self.cad_n as f64 / chunk
            } else {
                g / chunk
            };
        }

        let gate_music = mem.u32(self.lay.gate) == Some(1);
        let fxvol = mem.u8(self.lay.fxvol).unwrap_or(0) as u32;
        let tunevol = mem.u8(self.lay.tunevol).unwrap_or(0) as u32;

        // One-tick-delayed rendering: this hook closes the book on the
        // guest's PREVIOUS tick — its start positions (last snapshot),
        // its true per-voice steps (position telemetry over this gap),
        // and both gain endpoints are now all known exactly. The render
        // set below therefore reproduces tick N-1 verbatim, attacks
        // included, at the cost of one tick of stream latency (which
        // also matches the guest's own mix-to-DMA pipeline lag).
        let prev_snap = self.pending;

        let mut n = 0usize;
        let groups: [(u32, u32, bool); 2] = [
            (self.lay.fx_base, self.lay.fx_count, false),
            (self.lay.music_base, self.lay.music_count, true),
        ];
        for (base, cnt, is_music) in groups {
            for i in 0..cnt {
                if n >= MAX_VOICES {
                    break;
                }
                let va = base + i * V_STRIDE;
                let v = &mut self.pending[n];
                // The slot's previous content is this voice's snapshot
                // from the last mix hook (slot indices are stable per
                // address) — the baseline for position telemetry.
                let prev = prev_snap[n];
                n += 1;
                *v = Voice {
                    addr: va,
                    ..Voice::default()
                };
                let Some(state) = mem.u8(va + V_STATE) else {
                    continue;
                };
                if state != 0x11 && state != 0x12 {
                    continue;
                }
                if is_music && gate_music {
                    continue;
                }
                let grp = mem.u8(va + V_GRP).unwrap_or(0xFF) as i8;
                let mode = mem.u8(va + V_MODE).unwrap_or(0);
                let vol = mem.u8(va + V_VOL).unwrap_or(0) as u32;
                let env = mem.u32(va + V_ENV).unwrap_or(0);
                let pos = mem.u32(va + V_POS).unwrap_or(0);
                let frac = mem.u32(va + V_FRAC).unwrap_or(0) & 0x7F_FFFF;
                let Some(instr) = mem.u32(va + V_INSTR) else {
                    continue;
                };
                let (Some(end), Some(loop_len)) =
                    (mem.u32(instr + I_END), mem.u32(instr + I_LOOP_LEN))
                else {
                    self.bad_waves += 1;
                    continue;
                };
                // Sample data must be addressable; hostile pointers are
                // skipped, not followed.
                if mem.u8(pos).is_none() || end < pos {
                    self.bad_waves += 1;
                    continue;
                }
                // Gain chain (integer-exact mirror of the guest mixer):
                // master → channel volume (+1 bias) → optional second
                // channel table (later revision) → velocity → ADSR.
                let mut g = if grp < 0 {
                    fxvol
                } else {
                    let ch = (grp as u32) & 0xF;
                    let cv = mem.u8(self.lay.chanvol + ch).unwrap_or(0) as u32;
                    let mut g = (tunevol * (cv + 1)) >> 7;
                    if self.lay.chanvol2 != 0 {
                        let cv2 = mem.u8(self.lay.chanvol2 + ch).unwrap_or(0x80) as u32;
                        g = (g * cv2) >> 7;
                    }
                    g
                };
                g = (g * vol) >> 7;
                g = ((g as u64 * env as u64) >> 19) as u32;
                v.pos = pos as f64 + frac as f64 / 8388608.0;
                v.guest_pos = v.pos;
                v.end = end as f64;
                v.loop_len = loop_len;
                v.looping = matches!(mode, 2 | 4);
                v.instr = instr;
                v.gain = g as f32 / 128.0;
                v.on = true;
                // Per-voice pitch from guest position telemetry: the
                // step cache at +0x08 is only truthful on the steady
                // music path — the jingle/tune path computes its pitch
                // elsewhere. The guest's own position advance since the
                // previous tick IS the per-grid-sample step, no rate
                // table or chunk knowledge needed.
                v.grid_step = f64::NAN;
                if (256..=4096).contains(&gap) && prev.addr == va && prev.on && prev.instr == instr
                {
                    let mut dpos = v.guest_pos - prev.guest_pos;
                    // Loop wrap: the guest rewinds by loop_len.
                    let mut wraps = 0;
                    while dpos < 0.0 && prev.looping && prev.loop_len != 0 && wraps < 4 {
                        dpos += prev.loop_len as f64;
                        wraps += 1;
                    }
                    // Accept only a plausible forward advance (a
                    // backward jump is a note restart). The
                    // denominator is the SMOOTHED tick length, not the
                    // raw gap: the guest advanced dpos over a whole
                    // number of frame-locked ticks, so the jitter in
                    // when we observed the hooks must not leak into
                    // the step (it was the per-tick pitch wobble).
                    // round(gap / tick) keeps multi-tick spans
                    // (missed hooks) honest.
                    if dpos > 0.0 && dpos < gap as f64 * 4.0 {
                        let denom = if self.cad_n >= 8 {
                            let tick = self.cad_sum / self.cad_n as f64;
                            (gap as f64 / tick).round().max(1.0) * tick
                        } else {
                            gap as f64
                        };
                        v.grid_step = dpos / denom;
                    }
                }
            }
        }
        self.count = n;

        // Assemble the render set for the coming window from the
        // previous tick's snapshot: exact pitch (telemetry), exact gain
        // endpoints (prev → current, 0 for a voice's final window).
        let render_ok = (256..=4096).contains(&gap);
        self.win_len = if render_ok { gap as f64 } else { f64::MAX };
        self.win_pos = 0.0;
        self.dbg_new_window = true;
        self.replay_cap = ((2.0 * chunk) as usize).min(self.replay.len());
        // Any hook = ticking resumed: a later starvation must re-enter
        // replay through the synth (back-to-back outages separated by
        // one hook otherwise loop a stale ring that misses the chunk
        // this hook just committed). The unrenderable resume window
        // (win_len = MAX) keeps playing the ring below.
        self.replaying = false;
        for i in 0..n {
            let prev = prev_snap[i];
            let cur = self.pending[i];
            // The render voice still in this slot integrated to its
            // final position over the window that just finished — the
            // basis for the phase-continuous carry below.
            let fin = self.voices[i];
            let r = &mut self.voices[i];
            *r = Voice::default();
            if !render_ok || !prev.on || prev.addr != cur.addr {
                continue;
            }
            let step = if cur.grid_step.is_finite() {
                cur.grid_step
            } else if prev.grid_step.is_finite() {
                prev.grid_step
            } else {
                prev.step / self.mix_step
            };
            // NaN/inf step must take this branch (skip the voice).
            if !(step.is_finite() && step > 0.0) {
                continue;
            }
            *r = prev;
            r.dead = false;
            r.gain = prev.gain;
            r.gain_target = if cur.on { cur.gain } else { 0.0 };
            r.on = true;
            // Phase-continuous resampling. The guest is click-free
            // because it integrates a continuous position; we get only
            // per-hook position snapshots, and snapping pos to each one
            // steps the output by the per-window drift — our integrated
            // advance never lands exactly on the next snapshot (the
            // guest's own frac-accumulator rounds ~1 sample over a
            // chunk, and that gap is independent of how exactly we
            // pick the rate — tested: the exact cache rate does NOT
            // close it). That snap is a ~60 Hz click train, the
            // dominant periodic |Δ| by attribution (3.6× interior).
            // Instead START where the previous window's render actually
            // integrated to, keeping the position trajectory continuous,
            // and null the small telemetry drift across this window as a
            // rate nudge. The cost is a bounded sub-sample phase offset
            // vs the guest (no detune — the nudge zeroes the running
            // error each window); the gain is no click. A genuine large
            // jump (note restart / seek) is NOT drift — it snaps, and
            // that click is faithful.
            // Damp the drift correction: nulling the full error each
            // window would put the correction at the window rate (~60 Hz
            // — audible as roughness). Correcting a fraction per window
            // spreads it into a slow (~5-window) phase track, moving the
            // residual to sub-Hz where it's inaudible, at the cost of a
            // few samples of steady (constant, unmodulated) phase offset.
            const DAMP: f64 = 0.2;
            let dpos = step * self.win_len;
            let same_voice =
                fin.on && !fin.dead && fin.addr == prev.addr && fin.instr == prev.instr;
            let err = prev.guest_pos - fin.pos;
            if same_voice && fin.pos.is_finite() && err.abs() < 8.0 + 0.05 * dpos {
                r.pos = fin.pos;
                r.grid_step = step + DAMP * err / self.win_len;
                if self.dbg_disc {
                    self.dbg_err_n += 1;
                    self.dbg_err_sum += err.abs();
                    self.dbg_err_signed += err;
                }
            } else {
                r.pos = prev.guest_pos;
                r.grid_step = step;
                if self.dbg_disc {
                    self.dbg_snap_n += 1;
                }
            }
        }

        // Diagnostic (RECOMP_RDRV_TRACE): per-tick mixer snapshot.
        // Value "lo-hi" selects a hook range; any other value = first 600.
        let trace_tick = match std::env::var("RECOMP_RDRV_TRACE") {
            Ok(v) => {
                if let Some((lo, hi)) = v.split_once('-') {
                    let (lo, hi) = (
                        lo.parse::<u64>().unwrap_or(0),
                        hi.parse::<u64>().unwrap_or(u64::MAX),
                    );
                    (lo..=hi).contains(&self.hooks)
                } else {
                    self.hooks < 600
                }
            }
            Err(_) => false,
        };
        if trace_tick {
            eprintln!(
                "RDRV tick={} cursor={} gap={gap} chunk={chunk} mix_step={:.4} gate_music={gate_music} fxvol={fxvol} tunevol={tunevol}",
                self.hooks,
                audio_cursor / crate::mem::AUDIO_SAMPLE_CYCLES,
                self.mix_step
            );
            for v in self.pending[..n].iter() {
                if !v.on {
                    continue;
                }
                let state = mem.u8(v.addr + V_STATE).unwrap_or(0);
                let mode = mem.u8(v.addr + V_MODE).unwrap_or(0);
                eprintln!(
                    "  v@{:08x} st={state:02x} mode={mode} pos={:.1} end={:.1} loop={} gain={:.3} fx={} cache_step={:.4} tel_step={:.5} instr={:08x}",
                    v.addr,
                    v.pos,
                    v.end,
                    v.loop_len,
                    v.gain,
                    v.addr < self.lay.music_base,
                    v.step,
                    v.grid_step,
                    v.instr
                );
            }
            for r in self.voices[..n].iter() {
                if r.on {
                    eprintln!(
                        "  render v@{:08x} step={:.5} gain={:.3}->{:.3} pos={:.1}",
                        r.addr, r.grid_step, r.gain, r.gain_target, r.pos
                    );
                }
            }
        }
    }

    /// Synthesize one guest-rate chunk from the pending snapshot into
    /// the replay ring — the chunk the guest committed at the last
    /// hook that the one-tick-delayed render hasn't produced yet. The
    /// snapshot holds the exact start state of that tick (positions,
    /// telemetry steps, the gains canon mixed with).
    fn synth_replay_chunk(&mut self, mem: &MemView) {
        if self.replay_cap == 0 {
            return;
        }
        let chunk = self.replay_cap / 2;
        let mut vs: Vec<Voice> = Vec::new();
        for v in self.pending[..self.count].iter() {
            if !v.on {
                continue;
            }
            // Per-GUEST-sample step: telemetry (grid) scaled by the
            // grid/guest ratio, falling back to the +0x08 cache (which
            // is already guest-rate).
            let per_guest = if v.grid_step.is_finite() {
                v.grid_step * self.mix_step
            } else {
                v.step
            };
            if !(per_guest.is_finite() && per_guest > 0.0) {
                continue;
            }
            let mut c = *v;
            c.pos = v.guest_pos;
            c.grid_step = per_guest;
            c.dead = false;
            vs.push(c);
        }
        for _ in 0..chunk {
            let mut sum = 0.0f32;
            for c in vs.iter_mut() {
                if !c.dead {
                    sum += sample_voice(c, mem) * c.gain;
                }
            }
            self.replay_w = (self.replay_w + 1) % self.replay_cap;
            self.replay[self.replay_w] = sum.clamp(-1.0, 127.0 / 128.0);
            self.replay_r = self.replay_w;
        }
    }

    /// Advance-entry hook: the guest mixer cached each rendered voice's
    /// effective 9.23 step at +0x08 (the advance pass consumes and
    /// zeroes it right after this PC). Nonzero cache ⇔ the voice was
    /// actually mixed this chunk — budget and stealing included.
    fn steps_hook(&mut self, mem: &MemView, audio_cursor: u64, key: u32) {
        // The music pass runs first and sees both arrays intact; the fx
        // pass only commits when music didn't run this tick (paused).
        if key == self.lay.adv_fx
            && audio_cursor.saturating_sub(self.steps_cursor) < crate::mem::AUDIO_SAMPLE_CYCLES * 64
        {
            return;
        }
        self.steps_cursor = audio_cursor;
        for i in 0..self.count {
            let v = &mut self.pending[i];
            if v.on {
                let step = mem.u32(v.addr + V_STEP).unwrap_or(0);
                v.step = step as f64 / 8388608.0;
                v.on = step != 0;
                // First tick of a note has no position telemetry yet —
                // fall back to the cache step on the calibrated grid.
                if v.grid_step.is_nan() {
                    v.grid_step = v.step / self.mix_step;
                }
                if std::env::var_os("RECOMP_RDRV_TRACE").is_some() && self.hooks < 600 && v.on {
                    eprintln!(
                        "  step v@{:08x} raw={step:#010x} step={:.4} grid_step={:.5} key_is_fx={}",
                        v.addr,
                        v.step,
                        v.grid_step,
                        key == self.lay.adv_fx
                    );
                }
            }
        }
        // The render set was assembled at the mix hook from the
        // previous tick; the cache pickup above only feeds the NEXT
        // tick's prev.on / fallback step.
    }

    /// Dispatch-loop entry: PC landed on one of the hook keys.
    pub fn frame_hook(&mut self, mem: &MemView, audio_cursor: u64, key: u32) {
        if !self.active || !self.engaged {
            return;
        }
        if key == self.lay.mix_all {
            self.mix_hook(mem, audio_cursor);
        } else {
            self.steps_hook(mem, audio_cursor, key);
        }
    }

    pub fn live(&self) -> bool {
        self.active && self.engaged && self.vf.proven
    }

    pub fn gain(&self) -> f32 {
        self.vf.gain()
    }

    pub fn last_correlation(&self) -> (f32, f32) {
        self.vf.last()
    }

    /// Render one grid sample: mono sum routed to both sides, bus float
    /// scale (one full-scale FIFO DAC = 0.25). `canon` is the live FIFO
    /// DAC pair — this family feeds FIFO A only.
    pub fn render(&mut self, mem: &MemView, canon: [i8; 2], audio_cursor: u64) -> (f32, f32) {
        // Starvation: ticks stopped (the game stopped calling the
        // mixer — observed 1.4 s outages; also song changes, loads,
        // driver rebuild). "Layout valid" and "ticking" are separate
        // states: stay ENGAGED — the old disengage flapped against the
        // probe at sample cadence (re-engage had no cursor baseline,
        // so the next render call disengaged again; every starvation
        // became a long dropout with the tap dripping 1-of-16
        // samples). The hardware-faithful output during an outage is
        // the FIFO DMA's own behavior: it keeps cycling the two-chunk
        // output ring, looping the last committed chunks (a fresh
        // note-on becomes a sustained tone). Replay our check-stream
        // history of those chunks — output, check copy, and verifier
        // all follow it, so outages stay proven instead of striking.
        // While starved, periodically re-locate the blob: a rebuild
        // can move it, which moves the hook PCs; only a vanished blob
        // (true shutdown) disengages back to the probe.
        let since_hook =
            audio_cursor.saturating_sub(self.last_hook_cursor) / crate::mem::AUDIO_SAMPLE_CYCLES;
        let tick = if self.cad_n >= 8 {
            self.cad_sum / self.cad_n as f64
        } else {
            1100.0
        };
        // 1.25 ticks ≈ the rendered window is exhausted AND the guest's
        // DMA has moved on to the chunk committed at the last hook —
        // which our one-tick-delayed render never produced (its ring
        // holds chunks N-1 and N, our history N-2 and N-1; at a
        // note-onset outage that missing chunk IS the note — observed:
        // a 1.4 s outage right after a bass note-on). Synthesize chunk
        // N forward from the pending snapshot (the exact start state
        // of tick N) and start the replay on it, matching the DMA.
        if self.hook_seen && since_hook as f64 > 1.25 * tick {
            if !self.replaying {
                self.synth_replay_chunk(mem);
                if self.replay_cap > 0 {
                    let chunk = self.replay_cap / 2;
                    self.replay_r = (self.replay_w + self.replay_cap - chunk) % self.replay_cap;
                }
                self.replaying = true;
            }
            self.count = 0;
            for v in self.voices.iter_mut() {
                v.on = false;
            }
            if since_hook > 3 * 1100 {
                self.engage_backoff = self.engage_backoff.wrapping_add(1);
                if self.engage_backoff % 1024 == 0 {
                    match crate::engine::find(mem.iwram, &NOTE_ON_SIG, 0)
                        .and_then(|off| harvest(mem, 0x0300_0000 + off as u32))
                    {
                        Some(lay) => self.lay = lay,
                        None => {
                            if std::env::var_os("RECOMP_RDRV_TRACE").is_some() {
                                eprintln!(
                                    "rdrv: blob gone at cursor={} hooks={} — disengage",
                                    audio_cursor / crate::mem::AUDIO_SAMPLE_CYCLES,
                                    self.hooks
                                );
                            }
                            self.engaged = false;
                        }
                    }
                }
            }
        }
        // Replay also covers the unrenderable window right after a
        // resume hook (gap out of range -> win_len MAX, no render set
        // yet): the ring bridges the one tick until fresh data flows,
        // instead of a 16 ms notch against canon's fresh chunk.
        if self.replaying || (self.hook_seen && self.win_len == f64::MAX) {
            if self.replay_cap == 0 {
                return (0.0, 0.0);
            }
            self.chk_acc -= 1.0;
            if self.chk_acc <= 0.0 {
                self.replay_r = (self.replay_r + 1) % self.replay_cap;
                self.chk_hold = self.replay[self.replay_r];
                self.chk_acc += self.mix_step;
            }
            let canon_ab = (canon[0] as f32 / 128.0, canon[1] as f32 / 128.0);
            let _ = self.vf.judge(canon_ab, (self.chk_hold, 0.0));
            let m = self.chk_hold * 0.25;
            return (m, m);
        }
        let gain = self.vf.gain();
        // Envelope ramp position within the window (both gain endpoints
        // are guest ground truth; the lerp removes the per-tick zipper
        // the 8-bit canon mixer steps through).
        let t = (self.win_pos / self.win_len).min(1.0) as f32;
        self.win_pos += 1.0;
        let mut sum = 0.0f32;
        let mut any_wrap = false;
        let mut any_death = false;
        for v in self.voices[..self.count].iter_mut() {
            if !v.on || v.dead {
                continue;
            }
            let g = v.gain + (v.gain_target - v.gain) * t;
            let mut wrapped = false;
            let was_dead = v.dead;
            sum += sample_voice_dbg(v, mem, &mut wrapped) * g * gain;
            any_wrap |= wrapped;
            any_death |= v.dead && !was_dead;
        }
        if self.dbg_disc {
            let out = sum * 0.25;
            let step = (out - self.dbg_prev_out).abs() as f64;
            self.dbg_prev_out = out;
            let cls = if self.dbg_new_window {
                0
            } else if any_death {
                2
            } else if any_wrap {
                1
            } else {
                3
            };
            self.dbg[cls].0 += 1;
            self.dbg[cls].1 += step;
            self.dbg_new_window = false;
        }
        // Canon-domain check copy: the guest mixes on its own grid and
        // saturates the SUM to s8 — one stream-level ZOH at the guest
        // rate, clamped, against FIFO A (FIFO B stays silent on both
        // sides of the comparison). Fed only while the render window
        // is real (win_len finite): the tick right after an engage or
        // a starvation resume renders silence with no snapshot behind
        // it, and pairing that silence against live canon skews the
        // level ratio (it mis-adopted auto-gain on a chain that is
        // integer-exact).
        if self.win_len != f64::MAX {
            self.chk_acc -= 1.0;
            if self.chk_acc <= 0.0 {
                self.chk_hold = sum.clamp(-1.0, 127.0 / 128.0);
                self.chk_acc += self.mix_step;
                // Replay history: the most recent 2×chunk guest-rate
                // samples — what the guest's output ring would hold.
                // The read cursor trails the write cursor so a replay
                // starts at the oldest sample and loops chronologically.
                if self.replay_cap > 0 {
                    self.replay_w = (self.replay_w + 1) % self.replay_cap;
                    self.replay[self.replay_w] = self.chk_hold;
                    self.replay_r = self.replay_w;
                }
            }
            let canon_ab = (canon[0] as f32 / 128.0, canon[1] as f32 / 128.0);
            let _ = self.vf.judge(canon_ab, (self.chk_hold, 0.0));
        }
        let m = sum * 0.25;
        (m, m)
    }
}

impl Layout {
    fn chunk_ptr_deref(&self, mem: &MemView) -> Option<u32> {
        mem.u32(self.chunk_ptr)
    }
}

fn in_iwram(v: u32) -> bool {
    (0x0300_0000..0x0300_8000).contains(&v)
}
fn in_rom(v: u32) -> bool {
    matches!(v >> 24, 0x08..=0x0D)
}

/// Decode `ldr rd, [pc, #±imm]` literals over [start, end) and return
/// their dereferenced pool values in instruction order.
fn pc_literals(mem: &MemView, start: u32, end: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut a = start;
    while a + 4 <= end {
        if let Some(w) = mem.u32(a) {
            if w & 0xFF7F_0000 == 0xE51F_0000 {
                let imm = w & 0xFFF;
                let lit = if w & 0x0080_0000 != 0 {
                    a + 8 + imm
                } else {
                    a + 8 - imm
                };
                if let Some(v) = mem.u32(lit) {
                    out.push(v);
                }
            }
        }
        a += 4;
    }
    out
}

/// Locate the mixer entries and harvest every global from the live
/// blob's own pc-relative literals. Returns None unless every required
/// piece is found and validates — a partial match never engages.
fn harvest(mem: &MemView, blob: u32) -> Option<Layout> {
    // Mix entry: `sub sp, #imm; str lr, [sp, #8]; ldr r5, [pc, ...]`.
    let mut mix_all = 0u32;
    let mut a = blob;
    while a < blob + 0x200 {
        let (Some(w0), Some(w1), Some(w2)) = (mem.u32(a), mem.u32(a + 4), mem.u32(a + 8)) else {
            return None;
        };
        if w0 & 0xFFFF_F000 == 0xE24D_D000 && w1 == 0xE58D_E008 && w2 & 0xFFFF_F000 == 0xE59F_5000 {
            mix_all = a;
            break;
        }
        a += 4;
    }
    if mix_all == 0 {
        return None;
    }
    // Advance/env entries share a 5-word prologue; exactly four hits in
    // order advance_fx, advance_music, env_fx, env_music.
    let mut adv = Vec::new();
    let mut a = blob;
    while a + 20 <= blob + 0x1800 && adv.len() < 4 {
        if mem.u32(a) == Some(0xE92D_4000)
            && mem.u32(a + 4).map(|w| w & 0xFFFF_F000) == Some(0xE59F_B000)
            && mem.u32(a + 8) == Some(0xE59B_B000)
            && mem.u32(a + 12).map(|w| w & 0xFFFF_F000) == Some(0xE59F_A000)
            && mem.u32(a + 16) == Some(0xE08A_A00B)
        {
            adv.push(a);
        }
        a += 4;
    }
    if adv.len() < 2 {
        return None;
    }
    let (adv_fx, adv_music) = (adv[0], adv[1]);

    // Globals from the mix entry's literal sequence: mixbuf, budget,
    // fxcount(ROM), fx_base, gate, musiccount(ROM), music_base, ...,
    // mixbuf again, chunk count. Identified by value class + the
    // repeated-mixbuf anchor rather than raw indices.
    let lits = pc_literals(mem, mix_all, adv_fx);
    if lits.len() < 8 || !in_iwram(lits[0]) {
        return None;
    }
    let mixbuf = lits[0];
    let fx_i = lits.iter().position(|&v| in_rom(v))?;
    let fx_cnt_ptr = lits[fx_i];
    let fx_base = *lits[fx_i + 1..].iter().find(|&&v| in_iwram(v))?;
    let mus_i = fx_i
        + 1
        + lits[fx_i + 1..]
            .iter()
            .position(|&v| in_rom(v) && v != fx_cnt_ptr)?;
    let mus_cnt_ptr = lits[mus_i];
    let music_base = *lits[mus_i + 1..].iter().find(|&&v| in_iwram(v))?;
    let gate = *lits[fx_i + 1..mus_i]
        .iter()
        .rev()
        .find(|&&v| in_iwram(v) && v != fx_base)?;
    let rep = 1 + lits[1..].iter().position(|&v| v == mixbuf)?;
    let chunk_ptr = *lits.get(rep + 1).filter(|&&v| in_iwram(v))?;
    let fx_count = mem.u32(fx_cnt_ptr).filter(|c| c % V_STRIDE == 0)? / V_STRIDE;
    let music_count = mem.u32(mus_cnt_ptr).filter(|c| c % V_STRIDE == 0)? / V_STRIDE;
    if fx_count == 0 || fx_count > 16 || music_count == 0 || music_count > 112 {
        return None;
    }
    if !(64..=512).contains(&mem.u32(chunk_ptr)?) {
        return None;
    }

    // The per-voice routine is the single bl inside the mix entry; its
    // first literals are FX master, tune master, channel volume table —
    // and on the later revision (two `ldrb _, [_, r6]` loads instead of
    // one) a fourth: the extra channel table.
    let mut mix_voice = 0u32;
    let mut a = mix_all;
    while a < adv_fx {
        if let Some(w) = mem.u32(a) {
            if w & 0xFF00_0000 == 0xEB00_0000 {
                let imm = ((w & 0x00FF_FFFF) << 8) as i32 >> 8;
                let t = a.wrapping_add(8).wrapping_add((imm * 4) as u32);
                if t > blob && t < blob + 0x1800 {
                    mix_voice = t;
                    break;
                }
            }
        }
        a += 4;
    }
    if mix_voice == 0 {
        return None;
    }
    let mv_lits = pc_literals(mem, mix_voice, mix_voice + 0x80);
    if mv_lits.len() < 3 || !mv_lits[..3].iter().all(|&v| in_iwram(v)) {
        return None;
    }
    let mut ldrb_r6 = 0;
    let mut a = mix_voice;
    while a < mix_voice + 0x100 {
        if mem.u32(a).map(|w| w & 0xFFF0_0FFF) == Some(0xE7D0_0006) {
            ldrb_r6 += 1;
        }
        a += 4;
    }
    let chanvol2 = if ldrb_r6 >= 2 {
        *mv_lits.get(3).filter(|&&v| in_iwram(v))?
    } else {
        0
    };

    // Inner-loop sanity: the interp/accumulate opcode run must appear
    // (store + accumulate variants) or this is not the family we know.
    let mut inner = 0;
    let mut a = blob;
    while a + 16 <= blob + 0x1800 {
        if mem.u32(a) == Some(0xE1D0_10D0)
            && mem.u32(a + 4) == Some(0xE1D0_20D1)
            && mem.u32(a + 8) == Some(0xE042_2001)
            && mem.u32(a + 12) == Some(0xE003_0296)
        {
            inner += 1;
        }
        a += 4;
    }
    if inner < 2 {
        return None;
    }

    Some(Layout {
        mix_all,
        adv_music,
        adv_fx,
        fx_base,
        fx_count,
        music_base,
        music_count,
        gate,
        chunk_ptr,
        fxvol: mv_lits[0],
        tunevol: mv_lits[1],
        chanvol: mv_lits[2],
        chanvol2,
    })
}

/// Advance and fetch one voice sample (s8/128 units) with linear
/// interpolation, mirroring the guest inner loop: loop modes rewind by
/// the loop length at the end address; one-shots die there. Per-chunk
/// resync bounds any drift to one chunk.
fn sample_voice(v: &mut Voice, mem: &MemView) -> f32 {
    let mut ignore = false;
    sample_voice_dbg(v, mem, &mut ignore)
}

fn sample_voice_dbg(v: &mut Voice, mem: &MemView, wrapped: &mut bool) -> f32 {
    let mut guard = 0;
    while v.pos >= v.end {
        guard += 1;
        if !v.looping || v.loop_len == 0 || guard > 8 {
            v.dead = true;
            return 0.0;
        }
        v.pos -= v.loop_len as f64;
        *wrapped = true;
    }
    let base = v.pos.floor();
    let frac = (v.pos - base) as f32;
    let a0 = base as i64 as u32;
    let s0 = mem.u8(a0).map(|b| b as i8 as f32);
    let s1 = mem.u8(a0.wrapping_add(1)).map(|b| b as i8 as f32);
    let s = match (s0, s1) {
        (Some(x), Some(y)) => (x + (y - x) * frac) / 128.0,
        (Some(x), None) => x / 128.0,
        _ => {
            v.dead = true;
            return 0.0;
        }
    };
    v.pos += v.grid_step;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_gates_on_blob_signature() {
        let mut rom = vec![0u8; 0x1000];
        assert!(detect(&rom).is_none());
        rom[0x200..0x210].copy_from_slice(&NOTE_ON_SIG);
        assert_eq!(detect(&rom).unwrap().blob_rom_off, 0x200);
    }

    #[test]
    fn voice_loop_rewind_and_one_shot() {
        let ewram = vec![0u8; 4];
        let iwram = vec![0u8; 4];
        let mut rom = vec![0u8; 0x100];
        for (i, b) in rom.iter_mut().enumerate() {
            *b = (i as u8) & 0x3F;
        }
        let mem = MemView {
            rom: &rom,
            ewram: &ewram,
            iwram: &iwram,
        };
        let mut v = Voice {
            on: true,
            pos: 0x0800_0000u32 as f64,
            end: 0x0800_0010u32 as f64,
            loop_len: 8,
            looping: true,
            step: 3.0,
            grid_step: 3.0,
            gain: 1.0,
            ..Voice::default()
        };
        // Loop: rewinds by loop_len at end, stays alive and in range.
        for _ in 0..64 {
            let s = sample_voice(&mut v, &mem) * 128.0;
            assert!((0.0..16.0).contains(&s), "sample {s}");
        }
        assert!(!v.dead);
        // One-shot: dies at the end address.
        let mut v2 = Voice {
            looping: false,
            ..v
        };
        v2.pos = 0x0800_000Eu32 as f64;
        v2.end = 0x0800_0010u32 as f64;
        for _ in 0..4 {
            sample_voice(&mut v2, &mem);
        }
        assert!(v2.dead);
    }

    #[test]
    fn literal_decoder_reads_pool_values() {
        // ldr r0,[pc,#4] at IWRAM+0 → pool at +12; ldr r1,[pc,#-8]
        // U=0 at +4 → pool at +4 (self-referential word, value below).
        let mut iwram = vec![0u8; 0x40];
        iwram[0..4].copy_from_slice(&0xE59F_0004u32.to_le_bytes());
        iwram[4..8].copy_from_slice(&0xE51F_1008u32.to_le_bytes());
        iwram[12..16].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let rom = vec![0u8; 4];
        let ewram = vec![0u8; 4];
        let mem = MemView {
            rom: &rom,
            ewram: &ewram,
            iwram: &iwram,
        };
        let lits = pc_literals(&mem, 0x0300_0000, 0x0300_0008);
        assert_eq!(lits, vec![0xDEAD_BEEF, 0xE51F_1008]);
    }
}
