//! GAX v1-era (2001 lineage) sound-driver HLE shadow mixer.
//!
//! The v1 driver splits work across two IRQs: the VBlank tick runs the
//! bytecode sequencer and envelopes, writing per-voice **28-byte mix
//! entries** (position/end/loop pointers, 4.12 step, L/R volume bytes,
//! surround + echo-send flags); a Timer-1 IRQ then renders 128-sample
//! chunks from ROM-resident ARM mixer functions that read NOTHING but
//! those entries. That makes the shadow surface minimal: hook the
//! commit function (once per chunk, all entries final, positions
//! updated in place by the guest — so the shadow RESYNCS exactly every
//! chunk and cannot drift), re-render the same voices in float on the
//! tap grid, and verify differentially like the MP2K shadow.
//!
//! v1 is stereo with FIFO A = LEFT (opposite of the MP2K convention),
//! signed 8-bit samples, optional linear interpolation, and a simple
//! feedback-delay echo whose voices route to a separate accumulator.

use crate::mp2k::MemView;
use crate::shadow::{Judgement, Verifier};

/// Thumb prefix of the pitch-computation routine — present bit-exact
/// in every known build of the lineage (including recompiled ones
/// whose control layer differs); its literal pool holds the global
/// state pointer cell.
const CALC_NOTE_SIG: [u8; 32] = [
    0x30, 0xB5, 0x04, 0x1C, 0x09, 0x06, 0x0B, 0x0E, 0x19, 0x1C, 0x12, 0x06, 0x15, 0x0E, 0xA0, 0x7A,
    0x00, 0x2D, 0x16, 0xD1, 0x83, 0x42, 0x11, 0xD0, 0x22, 0x89, 0x98, 0x42, 0x04, 0xD2, 0x01, 0x49,
];

/// ARM prologues of the chunk-commit functions (stereo / mono) — used
/// as the runtime magic check on the function pointers the guest
/// installs, and to learn the output mode. The commit entry is the
/// hook PC: it runs exactly once per 128-sample chunk, after every
/// mix entry is final.
const COMMIT_STEREO_SIG: [u8; 16] = [
    0xF7, 0x43, 0x2D, 0xE9, 0x00, 0xE0, 0xA0, 0xE3, 0x20, 0x90, 0xA0, 0xE3, 0xF4, 0x40, 0xD0, 0xE0,
];
const COMMIT_MONO_SIG: [u8; 16] = [
    0xF3, 0x43, 0x2D, 0xE9, 0x00, 0xE0, 0xA0, 0xE3, 0x20, 0x90, 0xA0, 0xE3, 0xF0, 0x40, 0xD0, 0xE1,
];
/// Shared ARM prologue of all four voice-mix variants (magic check).
const MIX_SIG: [u8; 16] = [
    0xFE, 0x5F, 0x2D, 0xE9, 0x04, 0x20, 0x90, 0xE5, 0x08, 0xA0, 0x90, 0xE5, 0xB6, 0xB1, 0xD0, 0xE1,
];
/// Shared ARM prologue of the echo-processor variants.
const ECHO_SIG: [u8; 16] = [
    0xFE, 0x5F, 0x2D, 0xE9, 0x00, 0x20, 0x90, 0xE5, 0x0C, 0x30, 0x90, 0xE5, 0x08, 0x40, 0x90, 0xE5,
];

const MAX_VOICES: usize = 16;

/// One mix entry as read at a commit hook (post-chunk state). Two
/// consecutive snapshots bracket one chunk exactly: start position,
/// guest-true per-voice advance, and both gain endpoints.
#[derive(Clone, Copy, Default)]
struct SnapV {
    on: bool,
    /// Position including the 12-bit fraction.
    pos: f64,
    end: u32,
    loop_ptr: u32,
    loop_len: u32,
    /// Decoded 4.12 step (source samples per guest output sample).
    step: f64,
    gl: f32,
    gr: f32,
    echo_send: bool,
}

/// Detection result: the lineage gate (pitch-routine match). The
/// live state block is located at runtime by scanning guest RAM for
/// its self-verifying shape — recompiled builds shuffle literal
/// pools, but the installed mixer-pointer quartet cannot lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GaxV1Sig {
    pub calc_note_off: usize,
}

pub fn detect_v1(rom: &[u8]) -> Option<GaxV1Sig> {
    crate::engine::find(rom, &CALC_NOTE_SIG, 0).map(|pos| GaxV1Sig { calc_note_off: pos })
}

/// One rendered voice for the current window — chunk N played back
/// verbatim during [hook N, hook N+1): start position from the
/// previous snapshot, per-GRID-sample step from guest position
/// telemetry, gains lerped between the two snapshot endpoints.
#[derive(Clone, Copy)]
struct Voice {
    on: bool,
    pos: f64,
    end: f64,
    dir: f64,
    loop_ptr: u32,
    loop_len: u32,
    /// Source samples per GUEST output sample (position telemetry).
    step: f64,
    gl0: f32,
    gl1: f32,
    /// Right gains; negated when the surround flag is set.
    gr0: f32,
    gr1: f32,
    echo_send: bool,
    dead: bool,
}

impl Default for Voice {
    fn default() -> Voice {
        Voice {
            on: false,
            pos: 0.0,
            end: 0.0,
            dir: 1.0,
            loop_ptr: 0,
            loop_len: 0,
            step: 0.0,
            gl0: 0.0,
            gl1: 0.0,
            gr0: 0.0,
            gr1: 0.0,
            echo_send: false,
            dead: false,
        }
    }
}

/// Float port of the guest's feedback-delay echo. Cursor distance and
/// gains are re-read each hook; contents are our own (zero-seeded).
/// The stereo commit variant pairs with a stereo-INTERLEAVED delay
/// buffer (the 256-halfword echo processor): per-channel independent
/// delays, so the ring holds L/R pairs and cursors count pairs. The
/// mono variant's ring is one halfword per sample.
struct Echo {
    ring: Vec<(f32, f32)>,
    rd: usize,
    wr: usize,
    g_fb: f32,
    g_in: f32,
    g_wet: f32,
}

impl Echo {
    fn new() -> Echo {
        Echo {
            ring: Vec::new(),
            rd: 0,
            wr: 0,
            g_fb: 0.0,
            g_in: 0.0,
            g_wet: 0.0,
        }
    }
}

/// One v3 voice, snapshot of the transient 7-word mixer arg block
/// (everything precomputed by the guest: window, 21.11 position and
/// step, composite volume). Mono.
#[derive(Clone, Copy, Default)]
struct V3Voice {
    on: bool,
    /// Absolute byte position (u8 PCM) and window end.
    pos: f64,
    end: f64,
    /// Loop length in samples; 0 = one-shot.
    loop_len: f64,
    /// Source samples per guest output sample.
    step: f64,
    /// Composite volume /256.
    vol: f32,
    fx: bool,
    dead: bool,
}

/// Per-stream (music / fx) v3 state: guest mix rate and the
/// stream-level canon-domain ZOH.
#[derive(Clone, Copy)]
struct V3Stream {
    rate: f64,
    chk_hold: f32,
    chk_acc: f64,
    /// Two cascaded leaky integrators (the guest's optional LPF).
    lpf: [f32; 2],
}

impl Default for V3Stream {
    fn default() -> V3Stream {
        V3Stream {
            rate: 15769.0,
            chk_hold: 0.0,
            chk_acc: 0.0,
            lpf: [0.0; 2],
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    V1,
    V3,
}

pub struct GaxHle {
    mode: Mode,
    /// Guest address of the live SoundSystem block (found by scan).
    ss: u32,
    /// Dispatch key of the commit function (ARM) once engaged.
    hook_key: u32,
    pub active: bool,
    pub engaged: bool,
    stereo: bool,
    pub vf: Verifier,
    pub hooks: u64,
    pub stale_ticks: u64,
    pub bad_waves: u64,
    voices: [Voice; MAX_VOICES],
    count: usize,
    /// Grid samples per guest output sample (65536/mix_rate).
    mix_step: f64,
    echo_on: bool,
    echo: Echo,
    engage_backoff: u32,
    last_hook_cursor: u64,
    /// Hook-cadence statistics (mix-rate estimation).
    cad_sum: f64,
    cad_n: u32,
    cad_recent: [f64; 16],
    cad_idx: u32,
    /// Previous hook's entry snapshots (chunk start states).
    pending: [SnapV; MAX_VOICES],
    /// Render-window progress in grid samples (gain lerp phase).
    win_pos: f64,
    win_len: f64,
    /// Guest-tick accumulator and the held mix pair (ZOH to the grid).
    gtick: f64,
    mix_hold: (f32, f32),
    trace: bool,
    trace_acc: Vec<(usize, f64, f64)>,
    // --- v3 state ---
    /// Work block base; hook PCs: mixer normal, mixer ping-pong,
    /// quantizer (frame edge + canon compare point).
    v3_work: u32,
    v3_hooks: [u32; 3],
    v3_pending: Vec<V3Voice>,
    v3_voices: Vec<V3Voice>,
    v3_music: V3Stream,
    v3_fx: V3Stream,
    v3_lpf_depth: f32,
    v3_last_frame_cursor: u64,
}

impl GaxHle {
    pub fn new(_sig: GaxV1Sig) -> GaxHle {
        GaxHle {
            mode: Mode::V1,
            ss: 0,
            hook_key: 0,
            active: true,
            engaged: false,
            stereo: true,
            vf: Verifier::new(),
            hooks: 0,
            stale_ticks: 0,
            bad_waves: 0,
            voices: [Voice::default(); MAX_VOICES],
            count: 0,
            mix_step: 4.0,
            echo_on: false,
            echo: Echo::new(),
            engage_backoff: 0,
            last_hook_cursor: 0,
            cad_sum: 0.0,
            cad_n: 0,
            cad_recent: [0.0; 16],
            cad_idx: 0,
            pending: [SnapV::default(); MAX_VOICES],
            win_pos: 0.0,
            win_len: f64::MAX,
            gtick: 0.0,
            mix_hold: (0.0, 0.0),
            trace: std::env::var_os("RECOMP_GAX_TRACE").is_some(),
            trace_acc: Vec::new(),
            v3_work: 0,
            v3_hooks: [0; 3],
            v3_pending: Vec::new(),
            v3_voices: Vec::new(),
            v3_music: V3Stream::default(),
            v3_fx: V3Stream::default(),
            v3_lpf_depth: 0.0,
            v3_last_frame_cursor: 0,
        }
    }

    /// A v3-family shadow (banner-era driver): the work block is found
    /// at runtime by its 'GAX3' magic; mode/hooks resolve at engage.
    pub fn new_v3() -> GaxHle {
        let mut g = GaxHle::new(GaxV1Sig { calc_note_off: 0 });
        g.mode = Mode::V3;
        g
    }

    #[inline(always)]
    pub fn hook_match(&self, key: u32) -> bool {
        self.engaged
            && match self.mode {
                Mode::V1 => key == self.hook_key,
                Mode::V3 => self.v3_hooks.contains(&key),
            }
    }

    /// Periodic pre-engage probe (driven from the audio pump): once
    /// the guest driver has initialized, its state block carries four
    /// ROM function pointers whose bytes must match the known mixer
    /// family — simultaneously the magic check, the output-mode
    /// discovery, and the hook-PC source.
    pub fn try_engage(&mut self, mem: &MemView) {
        if self.engaged || !self.active {
            return;
        }
        // Cheap backoff: the driver may init seconds into boot.
        self.engage_backoff = self.engage_backoff.wrapping_add(1);
        if self.engage_backoff % 16 != 0 {
            return;
        }
        if self.mode == Mode::V3 {
            self.try_engage_v3(mem);
            return;
        }
        // Scan RAM for the state block's self-verifying shape: the
        // installed mixer-function pointers at +0xd0. A one-word fast
        // reject makes the sweep microseconds; it runs a few times per
        // second until the driver initializes, and cannot
        // false-positive past the code-byte checks. IWRAM every probe,
        // EWRAM (builds may allocate the block there) every fourth.
        let mut regions: Vec<(u32, u32)> = vec![(0x0300_0000, 0x8000)];
        if self.engage_backoff % 64 == 0 {
            regions.push((0x0200_0000, 0x4_0000));
        }
        for (base, len) in regions {
            for off in (0..len - 0xf4).step_by(4) {
                let ss = base + off;
                let Some(p0) = mem.u32(ss + 0xd0) else {
                    continue;
                };
                if p0 & 3 != 0 || !matches!(p0 >> 24, 2 | 3 | 8) {
                    continue;
                }
                if self.try_engage_block(mem, ss) {
                    return;
                }
            }
        }
    }

    fn try_engage_block(&mut self, mem: &MemView, ss: u32) -> bool {
        let dma = ss + 0xd0;
        let (Some(mix_a), Some(commit)) = (mem.u32(dma), mem.u32(dma + 0x08)) else {
            return false;
        };
        // The driver copies its ARM mixer functions into IWRAM at
        // init (speed); verify the prologue bytes wherever the
        // pointers land (RAM or ROM).
        let code_bytes = |addr: u32| -> Option<&[u8]> {
            if addr & 3 != 0 {
                return None;
            }
            mem.slice(addr, 16)
        };
        let Some(mix_pro) = code_bytes(mix_a) else {
            return false;
        };
        let Some(commit_pro) = code_bytes(commit) else {
            return false;
        };
        if mix_pro != MIX_SIG {
            return false;
        }
        let stereo = if commit_pro == COMMIT_STEREO_SIG {
            true
        } else if commit_pro == COMMIT_MONO_SIG {
            false
        } else {
            return false;
        };
        // Echo pointer is optional sanity (config may omit it).
        if let Some(echo_fn) = mem.u32(dma + 0x0c) {
            if let Some(p) = code_bytes(echo_fn) {
                if p != ECHO_SIG {
                    return false;
                }
            }
        }
        self.stereo = stereo;
        self.hook_key = commit & !3;
        self.ss = ss;
        self.engaged = true;
        if self.trace {
            let mix_b = mem.u32(dma + 0x04).unwrap_or(0);
            let echo_fn = mem.u32(dma + 0x0c).unwrap_or(0);
            eprintln!(
                "gaxengage ss={ss:08x} mixA={mix_a:08x} mixB={mix_b:08x} \
                 commit={commit:08x} echo={echo_fn:08x} stereo={stereo}"
            );
            for (name, p) in [("mixA", mix_a), ("mixB", mix_b), ("echo", echo_fn)] {
                if let Some(body) = mem.slice(p, 0x120) {
                    let hex: String = body.iter().map(|b| format!("{b:02x}")).collect();
                    eprintln!("gaxengage {name} body {hex}");
                }
            }
        }
        true
    }

    /// v3 engage: scan guest RAM for the work block's 'GAX3' magic,
    /// then validate the structural invariants and read the three
    /// RAM-resident code pointers (mixer normal/ping-pong, quantizer)
    /// — discovery, magic check, and hook PCs in one (the v1 trick).
    fn try_engage_v3(&mut self, mem: &MemView) {
        let magic = 0x4741_5833u32;
        let regions: [(u32, u32); 2] = [(0x0300_0000, 0x8000), (0x0200_0000, 0x4_0000)];
        for (base, len) in regions {
            let mut off = 0u32;
            while off + 0x80 <= len {
                let w = base + off;
                if mem.u32(w) == Some(magic) && self.v3_validate(mem, w) {
                    return;
                }
                off += 4;
            }
        }
    }

    fn v3_validate(&mut self, mem: &MemView, work: u32) -> bool {
        // irq_state in {1,2}; the three code pointers RAM-resident and
        // word-aligned; a sane mix rate in the active buffer header.
        let irq_state = mem.u32(work + 0x54).unwrap_or(0);
        if !(1..=2).contains(&irq_state) {
            return false;
        }
        let mut hooks = [0u32; 3];
        for (i, o) in [(0usize, 0x60u32), (1, 0x64), (2, 0x68)] {
            let Some(p) = mem.u32(work + o) else {
                return false;
            };
            if p & 3 != 0 || !matches!(p >> 24, 2 | 3) {
                return false;
            }
            hooks[i] = p & !3;
        }
        let rate_of = |hdr: u32| -> Option<f64> {
            let h = mem.u32(hdr)?;
            let r = mem
                .slice(h + 2, 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))?;
            ((4000..=48000).contains(&r)).then_some(r as f64)
        };
        let Some(music_rate) = rate_of(work + 0x24) else {
            return false;
        };
        self.v3_music.rate = music_rate;
        self.v3_fx.rate = rate_of(work + 0x28).unwrap_or(music_rate);
        self.v3_work = work;
        self.v3_hooks = hooks;
        self.engaged = true;
        true
    }

    /// v3 per-voice hook (mixer-loop entry): r0 points at the guest's
    /// transient arg block — window-end ptr, mix ptr, sample count,
    /// position (21.11, negative from window end), loop length (21.11),
    /// composite volume, step (21.11). Everything final, nothing to
    /// re-derive.
    fn v3_mix_hook(&mut self, mem: &MemView, r0: u32) {
        let Some(b) = mem.slice(r0, 28) else {
            self.stale_ticks += 1;
            return;
        };
        let w = |i: usize| u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        let end_ptr = w(0);
        let pos_neg = w(3) as i32;
        let loop_len = w(4);
        let vol = w(5);
        let step = w(6);
        if !matches!(end_ptr >> 24, 2 | 3 | 8..=0x0D) || step == 0 || vol == 0 {
            return;
        }
        // Music or fx pass, per the work block's render-pass id (0 =
        // single-rate/music, 2 = fx).
        let fx = mem.u8(self.v3_work + 0x5e) == Some(2);
        if self.v3_pending.len() < 32 {
            self.v3_pending.push(V3Voice {
                on: true,
                pos: end_ptr as f64 + pos_neg as f64 / 2048.0,
                end: end_ptr as f64,
                loop_len: loop_len as f64 / 2048.0,
                step: step as f64 / 2048.0,
                vol: (vol.min(1024) as f32) / 256.0,
                fx,
                dead: false,
            });
        }
    }

    /// v3 frame edge (quantizer entry): the pass's voices are all
    /// collected — swap them live and refresh rates/LPF depth.
    fn v3_frame_hook(&mut self, mem: &MemView, audio_cursor: u64) {
        self.hooks += 1;
        self.v3_last_frame_cursor = audio_cursor;
        // Keep the other stream's voices: passes alternate music/fx.
        let pass_fx = mem.u8(self.v3_work + 0x5e) == Some(2);
        self.v3_voices.retain(|v| v.fx != pass_fx);
        self.v3_voices.append(&mut self.v3_pending);
        self.v3_lpf_depth = mem
            .slice(self.v3_work + 0x74, 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as f32 / 256.0)
            .unwrap_or(0.0)
            .min(1.0);
        // Liveness: magic still present, else rescan.
        if mem.u32(self.v3_work) != Some(0x4741_5833) {
            self.engaged = false;
            self.v3_voices.clear();
            self.v3_pending.clear();
        }
    }

    /// Per-chunk hook (commit-function entry): every mix entry is
    /// final and positions are the guest's own — resync everything.
    pub fn frame_hook(&mut self, mem: &MemView, audio_cursor: u64, key: u32, r0: u32) {
        if !self.active || !self.engaged {
            return;
        }
        if self.mode == Mode::V3 {
            if key == self.v3_hooks[2] {
                self.v3_frame_hook(mem, audio_cursor);
            } else {
                self.v3_mix_hook(mem, r0);
            }
            return;
        }
        let _ = (key, r0);
        self.hooks += 1;
        let gap =
            audio_cursor.saturating_sub(self.last_hook_cursor) / crate::mem::AUDIO_SAMPLE_CYCLES;
        self.last_hook_cursor = audio_cursor;
        let ss = self.ss;
        // Mix rate from hook cadence — but as a long-run MEAN, not an
        // EMA: IRQ-latency jitter cancels in the mean (the DAC clock
        // is exact underneath), while an EMA's short memory turned it
        // into a ±0.5% wobble = audible warble plus a position snap at
        // every chunk resync. A 16-hook recent window re-seeds the
        // mean when the driver actually retunes its rate. The state
        // block's TM0 reload halfword (documented at +0xd0+0x22 in the
        // reference build) is adopted as the exact rate only when it
        // corroborates the cadence — earlier-lineage builds keep a
        // different value there (this title reads back 4096 Hz against
        // a measured ~8.2 kHz cadence).
        if (64..=4096).contains(&gap) {
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
                    // Rate retune: restart the mean from the recent window.
                    self.cad_sum = recent * 16.0;
                    self.cad_n = 16;
                }
            }
            if self.cad_n >= 8 {
                let est = self.cad_sum / self.cad_n as f64 / 128.0;
                let reload = mem
                    .slice(ss + 0xd0 + 0x22, 2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .unwrap_or(0);
                let timer_step = (65536.0 - reload as f64) / 256.0;
                self.mix_step = if (timer_step / est - 1.0).abs() < 0.05 {
                    timer_step
                } else {
                    est
                };
            }
        }
        // Cheap liveness check: the installed commit pointer must
        // still be the hooked one (driver reinit/realloc -> rescan).
        if mem.u32(ss + 0xd8).map(|p| p & !3) != Some(self.hook_key) {
            self.stale_ticks += 1;
            self.engaged = false;
            return;
        }
        let Some(count) = mem.u8(ss) else {
            self.stale_ticks += 1;
            return;
        };
        self.count = (count as usize).min(MAX_VOICES);
        let Some(aux) = mem.u32(ss + 0xc4).filter(|p| matches!(p >> 24, 2 | 3)) else {
            self.stale_ticks += 1;
            return;
        };
        // Snapshot every entry (post-chunk state), then render the
        // chunk these snapshots just closed: start positions from the
        // previous snapshot, per-voice advance from the guest's own
        // position telemetry, gain endpoints from both snapshots.
        let prev_snap = self.pending;
        let mut cur = [SnapV::default(); MAX_VOICES];
        for (i, c) in cur.iter_mut().enumerate().take(self.count) {
            let Some(ep) = mem.u32(aux + 4 * i as u32) else {
                continue;
            };
            if ep == 0 || !matches!(ep >> 24, 2 | 3) {
                continue;
            }
            let Some(e) = mem.slice(ep, 0x1c) else {
                continue;
            };
            let u32at = |o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);
            let pos_ptr = u32at(0x04);
            let end_ptr = u32at(0x08);
            let step = u16::from_le_bytes([e[0x14], e[0x15]]) as f64 / 4096.0;
            let frac = (u16::from_le_bytes([e[0x16], e[0x17]]) & 0xfff) as f64 / 4096.0;
            let vol_l = e[0x18];
            let vol_r = e[0x19];
            let surround = e[0x1a] == 1;
            // Sample data must be addressable (ROM or RAM).
            if mem.slice(pos_ptr, 1).is_none() || step <= 0.0 {
                if prev_snap[i].on {
                    self.bad_waves += 1;
                }
                continue;
            }
            let gl = if vol_l == 0 {
                0.0
            } else {
                (vol_l as f32 + 1.0) / 256.0
            };
            let gr = if vol_r == 0 {
                0.0
            } else {
                (vol_r as f32 + 1.0) / 256.0
            };
            *c = SnapV {
                on: true,
                pos: pos_ptr as f64 + frac,
                end: end_ptr,
                loop_ptr: u32at(0x0c),
                loop_len: u32at(0x10),
                step,
                gl,
                gr: if surround { -gr } else { gr },
                echo_send: e[0x1b] != 0,
            };
        }
        self.pending = cur;

        // The chunk is always exactly 128 guest samples; the window it
        // plays over is its DAC span, immune to hook (IRQ) jitter.
        let chunk_grid = 128.0 * self.mix_step;
        self.win_len = chunk_grid;
        self.win_pos = 0.0;
        for i in 0..MAX_VOICES {
            let p = prev_snap[i];
            let c = cur[i];
            let v = &mut self.voices[i];
            *v = Voice::default();
            // Note identity: the loop region. The end pointer is
            // rewritten by the guest on every loop wrap, so it cannot
            // identify; a same-loop step change is a pitch slide, not
            // a restart.
            let same = p.on && c.on && p.loop_ptr == c.loop_ptr && p.loop_len == c.loop_len;
            let (start, src, gl0, gr0) = if same {
                (p.pos, c, p.gl, p.gr)
            } else if c.on {
                // New note this chunk: the mixer ran it a full chunk,
                // so its start is one chunk's advance behind (loop
                // wraps folded by the sampler's own machinery).
                let dir = if c.end as f64 >= c.pos { 1.0 } else { -1.0 };
                (c.pos - dir * c.step * 128.0, c, c.gl, c.gr)
            } else if p.on {
                // Voice gone: render its final chunk fading to zero.
                (p.pos, p, p.gl, p.gr)
            } else {
                continue;
            };
            // Per-voice advance from guest telemetry where available:
            // dpos across the chunk, forward-loop unwrapped. A failed
            // unwrap (ping-pong reflection, restart) falls back to the
            // decoded 4.12 step.
            let mut step = src.step;
            if same {
                let mut d = c.pos - p.pos;
                let mut wraps = 0;
                while d < 0.0 && c.loop_len != 0 && wraps < 4 {
                    d += c.loop_len as f64;
                    wraps += 1;
                }
                let exp = p.step * 128.0;
                if d > 0.0 && d < exp * 4.0 + 4.0 {
                    step = d / 128.0;
                    if self.trace && exp > 0.0 {
                        self.trace_acc.push((i, d / exp, p.step));
                    }
                }
            }
            // NaN/inf step must take this branch (skip the voice).
            if !(step.is_finite() && step > 0.0) {
                continue;
            }
            v.pos = start;
            v.end = src.end as f64;
            v.dir = if src.end as f64 >= start { 1.0 } else { -1.0 };
            v.loop_ptr = src.loop_ptr;
            v.loop_len = src.loop_len;
            v.step = step;
            v.gl0 = gl0;
            v.gr0 = gr0;
            v.gl1 = if c.on { c.gl } else { 0.0 };
            v.gr1 = if c.on { c.gr } else { 0.0 };
            v.echo_send = src.echo_send;
            v.dead = false;
            v.on = true;
        }

        if self.trace && self.hooks % 64 == 0 {
            let mut flags = String::new();
            for (i, c) in cur.iter().enumerate() {
                if !c.on {
                    continue;
                }
                let bwd = (c.end as f64) < c.pos;
                flags += &format!(
                    " v{i}[sur={} echo={} bwd={} loop={} step={:.3} gl={:.2} gr={:+.2}]",
                    (c.gr < 0.0) as u8,
                    c.echo_send as u8,
                    bwd as u8,
                    (c.loop_len != 0) as u8,
                    c.step,
                    c.gl,
                    c.gr,
                );
            }
            eprintln!(
                "gaxflags hooks={} echo_on={}{}",
                self.hooks, self.echo_on, flags
            );
        }
        if self.trace && self.hooks % 256 == 0 && !self.trace_acc.is_empty() {
            let mut by_slot: [(f64, f64, u32); MAX_VOICES] = [(0.0, 0.0, 0); MAX_VOICES];
            for &(s, r, st) in &self.trace_acc {
                by_slot[s].0 += r;
                by_slot[s].1 += st;
                by_slot[s].2 += 1;
            }
            let mut line = format!(
                "gaxtrace hooks={} mix_step={:.3} (rate {:.0} Hz):",
                self.hooks,
                self.mix_step,
                65536.0 / self.mix_step
            );
            for (s, &(rs, ss_, n)) in by_slot.iter().enumerate() {
                if n > 0 {
                    line += &format!(
                        " v{s}:r={:.3},step={:.3},n={n}",
                        rs / n as f64,
                        ss_ / n as f64
                    );
                }
            }
            eprintln!("{line}");
            self.trace_acc.clear();
        }

        // Echo configuration (gains share the guest's nonzero->+1 bias).
        self.echo_on = mem.u8(ss + 0x10e).is_some_and(|b| b != 0);
        if self.echo_on {
            let es = ss + 0xf4;
            let (base, wr, rd, end) = (
                mem.u32(es + 0x04).unwrap_or(0),
                mem.u32(es + 0x08).unwrap_or(0),
                mem.u32(es + 0x0c).unwrap_or(0),
                mem.u32(es + 0x10).unwrap_or(0),
            );
            let bias = |g: u16| {
                if g == 0 {
                    0.0
                } else {
                    (g as f32 + 1.0) / 65536.0
                }
            };
            let g16 = |a: u32| {
                mem.slice(a, 2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .unwrap_or(0)
            };
            self.echo.g_fb = bias(g16(es + 0x14));
            self.echo.g_in = bias(g16(es + 0x16));
            self.echo.g_wet = bias(g16(es + 0x18));
            // Stereo: the delay buffer is interleaved L/R halfwords, so
            // a sample of delay is FOUR bytes; mono: two.
            let stride = if self.stereo { 4 } else { 2 };
            let len = (end.saturating_sub(base) / stride) as usize;
            if len > 0 && len <= 1 << 17 {
                if self.echo.ring.len() != len {
                    self.echo.ring = vec![(0.0, 0.0); len];
                    self.echo.rd = 0;
                    self.echo.wr = 0;
                }
                if base != 0 && wr >= base && rd >= base {
                    self.echo.rd = ((rd - base) / stride) as usize % len;
                    self.echo.wr = ((wr - base) / stride) as usize % len;
                }
            } else {
                self.echo_on = false;
            }
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

    /// v3 render: two mono streams (music -> FIFO A, fx -> FIFO B,
    /// both routed to both speakers by the driver's SOUNDCNT_H). The
    /// self-check pairs each stream with its own FIFO.
    fn render_v3(&mut self, mem: &MemView, canon: [i8; 2], audio_cursor: u64) -> (f32, f32) {
        // Hook starvation: the driver rebuilds its RAM mixer code on
        // song/section changes, moving the hook PCs. The work block
        // still holds the live pointers — refresh from it; if even the
        // magic is gone, fall back to a full rescan.
        let starved = audio_cursor.saturating_sub(self.v3_last_frame_cursor)
            / crate::mem::AUDIO_SAMPLE_CYCLES
            > 3 * 1100;
        if starved {
            self.v3_last_frame_cursor = audio_cursor;
            self.v3_voices.clear();
            self.v3_pending.clear();
            if mem.u32(self.v3_work) == Some(0x4741_5833) {
                let mut ok = true;
                let mut hooks = [0u32; 3];
                for (i, o) in [(0usize, 0x60u32), (1, 0x64), (2, 0x68)] {
                    match mem.u32(self.v3_work + o) {
                        Some(p) if p & 3 == 0 && matches!(p >> 24, 2 | 3) => hooks[i] = p & !3,
                        _ => ok = false,
                    }
                }
                if ok {
                    self.v3_hooks = hooks;
                }
            } else {
                self.engaged = false;
            }
        }
        let gain = self.vf.gain();
        let (mut music, mut fx) = (0.0f32, 0.0f32);
        let m_scale = self.v3_music.rate / 65536.0;
        let f_scale = self.v3_fx.rate / 65536.0;
        for v in self.v3_voices.iter_mut() {
            if !v.on || v.dead {
                continue;
            }
            let s = v3_sample(v, mem, if v.fx { f_scale } else { m_scale });
            if v.fx {
                fx += s * v.vol * gain;
            } else {
                music += s * v.vol * gain;
            }
        }
        // Optional guest low-pass (two cascaded leaky integrators);
        // the canon stream includes it, so ours and the check must.
        if self.v3_lpf_depth > 0.0 {
            let a = (self.v3_lpf_depth * (0x334 as f32 / 256.0)).min(1.0);
            for (st, val) in [(&mut self.v3_music, &mut music), (&mut self.v3_fx, &mut fx)] {
                st.lpf[0] += (*val - st.lpf[0]) * a;
                st.lpf[1] += (st.lpf[0] - st.lpf[1]) * a;
                *val = st.lpf[1];
            }
        }
        // Stream-level canon-domain ZOH at each stream's guest rate.
        for (st, val) in [(&mut self.v3_music, music), (&mut self.v3_fx, fx)] {
            st.chk_acc -= 1.0;
            if st.chk_acc <= 0.0 {
                st.chk_hold = val;
                st.chk_acc += 65536.0 / st.rate;
            }
        }
        let sat = (
            self.v3_music.chk_hold.clamp(-1.0, 127.0 / 128.0),
            self.v3_fx.chk_hold.clamp(-1.0, 127.0 / 128.0),
        );
        let canon_ab = (canon[0] as f32 / 128.0, canon[1] as f32 / 128.0);
        let _ = self.vf.judge(canon_ab, sat);
        let m = (music + fx) * 0.25;
        (m, m)
    }

    /// Render one grid sample: (left, right) in bus float scale (one
    /// full-scale FIFO DAC = 0.25, matching the canon path). `canon`
    /// is the live FIFO DAC pair — v1 maps FIFO A = LEFT.
    pub fn render(&mut self, mem: &MemView, canon: [i8; 2], audio_cursor: u64) -> (f32, f32) {
        if self.mode == Mode::V3 {
            return self.render_v3(mem, canon, audio_cursor);
        }
        // The mix advances at GUEST ticks and HOLDS across the host
        // grid. The hold is deliberate, not a shortcut: at a ~8 kHz
        // mix rate most of the audible treble is ZOH image energy, and
        // the guest commit's s8 rail clamp is part of the authored
        // sound — a smooth full-rate render audibly dulls the title
        // (crossfading from canon to it sounded like a pitch drop).
        // What the enhanced stream still buys over canon: float mixing
        // and echo (no s8 truncation), exact jitter-free timing, and
        // telemetry-true pitch. The output IS the canon-domain check.
        self.win_pos += 1.0;
        self.gtick -= 1.0;
        if self.gtick <= 0.0 {
            self.gtick += self.mix_step;
            let gain = self.vf.gain();
            // Gain lerp phase across the chunk window (held at 1 if
            // the next hook is late — voices keep sampling smoothly).
            let t = if self.win_len > 0.0 {
                (self.win_pos / self.win_len).min(1.0) as f32
            } else {
                1.0
            };
            let (mut l, mut r) = (0.0f32, 0.0f32);
            let (mut echo_in_l, mut echo_in_r) = (0.0f32, 0.0f32);
            for v in self.voices.iter_mut() {
                if !v.on || v.dead {
                    continue;
                }
                let s = sample_voice(v, mem);
                let gl = v.gl0 + (v.gl1 - v.gl0) * t;
                let gr = v.gr0 + (v.gr1 - v.gr0) * t;
                if v.echo_send {
                    // Echo-send voices feed the delay line INSTEAD of
                    // the main mix, at their panned (signed) gains.
                    echo_in_l += s * gl * gain;
                    echo_in_r += s * gr * gain;
                } else {
                    l += s * gl * gain;
                    r += s * gr * gain;
                }
            }
            // Echo shares the tick; per-channel independent delays in
            // stereo mode (the guest's interleaved buffer never mixes
            // the channels), one line duplicated in mono mode.
            if self.echo_on && !self.echo.ring.is_empty() {
                let e = &mut self.echo;
                let n = e.ring.len();
                let (in_l, in_r) = if self.stereo {
                    (echo_in_l, echo_in_r)
                } else {
                    let m = (echo_in_l + echo_in_r) * 0.5;
                    (m, m)
                };
                let (dl, dr) = e.ring[e.rd];
                let wet = (e.g_wet * dl + e.g_in * in_l, e.g_wet * dr + e.g_in * in_r);
                let (wl, wr_) = e.ring[e.wr];
                e.ring[e.wr] = (in_l + e.g_fb * wl, in_r + e.g_fb * wr_);
                e.rd = (e.rd + 1) % n;
                e.wr = (e.wr + 1) % n;
                l += wet.0;
                r += wet.1;
            }
            if !self.stereo {
                let m = (l + r) * 0.5;
                l = m;
                r = m;
            }
            // The guest commit clamps every sample to the s8 rails.
            self.mix_hold = (l.clamp(-1.0, 127.0 / 128.0), r.clamp(-1.0, 127.0 / 128.0));
        }

        let (hl, hr) = self.mix_hold;
        // v1 FIFO A carries LEFT.
        let canon_lr = (canon[0] as f32 / 128.0, canon[1] as f32 / 128.0);
        match self.vf.judge(canon_lr, (hl, hr)) {
            Judgement::None | Judgement::Pass | Judgement::Fail { .. } => {}
        }
        (hl * 0.25, hr * 0.25)
    }
}

/// Advance and fetch one voice sample (s8/128 units) with linear
/// interpolation (matching the guest's interpolating mixer variants);
/// handles forward loops and the backward (ping-pong) leg by
/// reflection. Per-chunk resync bounds any semantic drift to one
/// chunk.
fn sample_voice(v: &mut Voice, mem: &MemView) -> f32 {
    if v.dead {
        return 0.0;
    }
    // Boundary handling first (position may arrive past the edge).
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 8 {
            v.dead = true;
            return 0.0;
        }
        if v.dir > 0.0 && v.pos >= v.end {
            if v.loop_ptr == 0 || v.loop_len == 0 {
                v.dead = true;
                return 0.0;
            }
            // Forward leg wraps to the loop region.
            let over = v.pos - v.end;
            v.pos = v.loop_ptr as f64 + over;
            v.end = (v.loop_ptr + v.loop_len) as f64;
        } else if v.dir < 0.0 && v.pos <= v.end {
            if v.loop_ptr == 0 || v.loop_len == 0 {
                v.dead = true;
                return 0.0;
            }
            // Backward leg reflects forward off its lower bound.
            let over = v.end - v.pos;
            v.pos = v.end + over;
            v.dir = 1.0;
            v.end = (v.loop_ptr + v.loop_len) as f64;
        } else {
            break;
        }
    }
    let base = v.pos.floor();
    let frac = (v.pos - base) as f32;
    let a0 = base as i64 as u32;
    let a1 = a0.wrapping_add(1);
    let s0 = mem.u8(a0).map(|b| b as i8 as f32);
    let s1 = mem.u8(a1).map(|b| b as i8 as f32);
    let s = match (s0, s1) {
        (Some(x), Some(y)) => (x + (y - x) * frac) / 128.0,
        (Some(x), None) => x / 128.0,
        _ => {
            v.dead = true;
            return 0.0;
        }
    };
    v.pos += v.dir * v.step;
    s
}

/// Advance and fetch one v3 voice sample (unsigned 8-bit PCM,
/// centered; window ends at `end`, loops back by `loop_len` samples or
/// dies). Linear interpolation; per-frame resync bounds drift.
fn v3_sample(v: &mut V3Voice, mem: &MemView, scale: f64) -> f32 {
    let mut guard = 0;
    while v.pos >= v.end {
        guard += 1;
        if v.loop_len <= 0.0 || guard > 8 {
            v.dead = true;
            return 0.0;
        }
        v.pos -= v.loop_len;
    }
    let base = v.pos.floor();
    let frac = (v.pos - base) as f32;
    let a0 = base as i64 as u32;
    let s0 = mem.u8(a0);
    let s1 = mem.u8(a0.wrapping_add(1)).or(s0);
    let out = match (s0, s1) {
        (Some(x), Some(y)) => {
            let x = x as f32 - 128.0;
            let y = y as f32 - 128.0;
            (x + (y - x) * frac) / 128.0
        }
        _ => {
            v.dead = true;
            return 0.0;
        }
    };
    v.pos += v.step * scale;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_v1_gates_on_signature() {
        let mut rom = vec![0u8; 0x1000];
        assert!(detect_v1(&rom).is_none());
        rom[0x400..0x400 + 32].copy_from_slice(&CALC_NOTE_SIG);
        assert_eq!(detect_v1(&rom).unwrap().calc_note_off, 0x400);
    }

    #[test]
    fn voice_forward_loop_and_one_shot() {
        let ewram = vec![0u8; 4];
        let iwram = vec![0u8; 4];
        let mut rom = vec![0u8; 0x100];
        for (i, b) in rom.iter_mut().enumerate() {
            *b = (i as u8) & 0x7f;
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
            dir: 1.0,
            loop_ptr: 0x0800_0008,
            loop_len: 8,
            step: 3.0,
            ..Voice::default()
        };
        // Looping: samples stay inside [8, 16) after wrap, voice alive.
        for _ in 0..64 {
            let s = sample_voice(&mut v, &mem) * 128.0;
            assert!((0.0..16.0).contains(&s), "sample {s}");
        }
        assert!(!v.dead);
        // One-shot: dies at the end.
        let mut v2 = Voice {
            loop_ptr: 0,
            loop_len: 0,
            ..v
        };
        v2.pos = 0x0800_000Eu32 as f64;
        v2.end = 0x0800_0010u32 as f64;
        for _ in 0..4 {
            sample_voice(&mut v2, &mem);
        }
        assert!(v2.dead);
    }
}
