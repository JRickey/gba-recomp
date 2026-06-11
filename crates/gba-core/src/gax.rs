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
    0x30, 0xB5, 0x04, 0x1C, 0x09, 0x06, 0x0B, 0x0E, 0x19, 0x1C, 0x12, 0x06, 0x15, 0x0E,
    0xA0, 0x7A, 0x00, 0x2D, 0x16, 0xD1, 0x83, 0x42, 0x11, 0xD0, 0x22, 0x89, 0x98, 0x42,
    0x04, 0xD2, 0x01, 0x49,
];

/// ARM prologues of the chunk-commit functions (stereo / mono) — used
/// as the runtime magic check on the function pointers the guest
/// installs, and to learn the output mode. The commit entry is the
/// hook PC: it runs exactly once per 128-sample chunk, after every
/// mix entry is final.
const COMMIT_STEREO_SIG: [u8; 16] = [
    0xF7, 0x43, 0x2D, 0xE9, 0x00, 0xE0, 0xA0, 0xE3, 0x20, 0x90, 0xA0, 0xE3, 0xF4, 0x40,
    0xD0, 0xE0,
];
const COMMIT_MONO_SIG: [u8; 16] = [
    0xF3, 0x43, 0x2D, 0xE9, 0x00, 0xE0, 0xA0, 0xE3, 0x20, 0x90, 0xA0, 0xE3, 0xF0, 0x40,
    0xD0, 0xE1,
];
/// Shared ARM prologue of all four voice-mix variants (magic check).
const MIX_SIG: [u8; 16] = [
    0xFE, 0x5F, 0x2D, 0xE9, 0x04, 0x20, 0x90, 0xE5, 0x08, 0xA0, 0x90, 0xE5, 0xB6, 0xB1,
    0xD0, 0xE1,
];
/// Shared ARM prologue of the echo-processor variants.
const ECHO_SIG: [u8; 16] = [
    0xFE, 0x5F, 0x2D, 0xE9, 0x00, 0x20, 0x90, 0xE5, 0x0C, 0x30, 0x90, 0xE5, 0x08, 0x40,
    0x90, 0xE5,
];

const MAX_VOICES: usize = 16;

/// Detection result: the lineage gate (pitch-routine match). The
/// live state block is located at runtime by scanning guest RAM for
/// its self-verifying shape — recompiled builds shuffle literal
/// pools, but the installed mixer-pointer quartet cannot lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GaxV1Sig {
    pub calc_note_off: usize,
}

pub fn detect_v1(rom: &[u8]) -> Option<GaxV1Sig> {
    find(rom, &CALC_NOTE_SIG).map(|pos| GaxV1Sig { calc_note_off: pos })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        match hay[i..].iter().position(|&b| b == first) {
            Some(o) => {
                i += o;
                if i + needle.len() > hay.len() {
                    return None;
                }
                if &hay[i..i + needle.len()] == needle {
                    return Some(i);
                }
                i += 1;
            }
            None => return None,
        }
    }
    None
}

/// One shadowed voice, mirrored from a 28-byte mix entry. Positions
/// are absolute guest byte addresses (sample data is s8); the guest
/// rewrites the entry in place each chunk, so every hook resyncs the
/// sampler exactly.
#[derive(Clone, Copy)]
struct Voice {
    on: bool,
    pos: f64,
    end: f64,
    dir: f64,
    loop_ptr: u32,
    loop_len: u32,
    /// Source samples per guest output sample (4.12 -> float).
    step: f64,
    gl: f32,
    /// Right gain; negated when the surround flag is set.
    gr: f32,
    echo_send: bool,
    dead: bool,
    /// Canon-domain check copy: guest-rate ZOH.
    chk_hold: f32,
    chk_acc: f64,
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
            gl: 0.0,
            gr: 0.0,
            echo_send: false,
            dead: false,
            chk_hold: 0.0,
            chk_acc: 0.0,
        }
    }
}

/// Float port of the guest's feedback-delay echo. Cursor distance and
/// gains are re-read each hook; contents are our own (zero-seeded).
struct Echo {
    ring: Vec<f32>,
    rd: usize,
    wr: usize,
    g_fb: f32,
    g_in: f32,
    g_wet: f32,
    wet_hold: f32,
    acc: f64,
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
            wet_hold: 0.0,
            acc: 0.0,
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
        V3Stream { rate: 15769.0, chk_hold: 0.0, chk_acc: 0.0, lpf: [0.0; 2] }
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
                let Some(p0) = mem.u32(ss + 0xd0) else { continue };
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
        let Some(mix_pro) = code_bytes(mix_a) else { return false };
        let Some(commit_pro) = code_bytes(commit) else { return false };
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
            let Some(p) = mem.u32(work + o) else { return false };
            if p & 3 != 0 || !matches!(p >> 24, 2 | 3) {
                return false;
            }
            hooks[i] = p & !3;
        }
        let rate_of = |hdr: u32| -> Option<f64> {
            let h = mem.u32(hdr)?;
            let r = mem.slice(h + 2, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))?;
            ((4000..=48000).contains(&r)).then_some(r as f64)
        };
        let Some(music_rate) = rate_of(work + 0x24) else { return false };
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
        // The guest renders exactly 128 samples per hook, so the hook
        // cadence IS the mix rate — self-calibrating, no timer-reload
        // semantics needed. EMA dampens IRQ-latency jitter.
        let gap = audio_cursor.saturating_sub(self.last_hook_cursor) / crate::mem::AUDIO_SAMPLE_CYCLES;
        self.last_hook_cursor = audio_cursor;
        if (64..=4096).contains(&gap) {
            let step = gap as f64 / 128.0;
            self.mix_step = if self.hooks < 8 {
                step
            } else {
                self.mix_step * 0.9 + step * 0.1
            };
        }
        let ss = self.ss;
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
        for i in 0..MAX_VOICES {
            if i >= self.count {
                self.voices[i].on = false;
                continue;
            }
            let v = &mut self.voices[i];
            let Some(ep) = mem.u32(aux + 4 * i as u32) else {
                v.on = false;
                continue;
            };
            if ep == 0 || !matches!(ep >> 24, 2 | 3) {
                v.on = false;
                continue;
            }
            let Some(e) = mem.slice(ep, 0x1c) else {
                v.on = false;
                continue;
            };
            let u32at = |o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);
            let pos_ptr = u32at(0x04);
            let end_ptr = u32at(0x08);
            let loop_ptr = u32at(0x0c);
            let loop_len = u32at(0x10);
            let step = u16::from_le_bytes([e[0x14], e[0x15]]) as f64 / 4096.0;
            let frac = (u16::from_le_bytes([e[0x16], e[0x17]]) & 0xfff) as f64 / 4096.0;
            let vol_l = e[0x18];
            let vol_r = e[0x19];
            let surround = e[0x1a] == 1;
            let echo_send = e[0x1b] != 0;
            // Sample data must be addressable (ROM or RAM).
            if mem.slice(pos_ptr, 1).is_none() || step <= 0.0 {
                if v.on {
                    self.bad_waves += 1;
                }
                v.on = false;
                continue;
            }
            let gl = if vol_l == 0 { 0.0 } else { (vol_l as f32 + 1.0) / 256.0 };
            let gr0 = if vol_r == 0 { 0.0 } else { (vol_r as f32 + 1.0) / 256.0 };
            v.pos = pos_ptr as f64 + frac;
            v.end = end_ptr as f64;
            v.dir = if end_ptr >= pos_ptr { 1.0 } else { -1.0 };
            v.loop_ptr = loop_ptr;
            v.loop_len = loop_len;
            v.step = step;
            v.gl = gl;
            v.gr = if surround { -gr0 } else { gr0 };
            v.echo_send = echo_send;
            v.dead = false;
            v.on = true;
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
            let bias = |g: u16| if g == 0 { 0.0 } else { (g as f32 + 1.0) / 65536.0 };
            let g16 = |a: u32| {
                mem.slice(a, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
            };
            self.echo.g_fb = bias(g16(es + 0x14));
            self.echo.g_in = bias(g16(es + 0x16));
            self.echo.g_wet = bias(g16(es + 0x18));
            let len = (end.saturating_sub(base) / 2) as usize;
            if len > 0 && len <= 1 << 17 {
                if self.echo.ring.len() != len {
                    self.echo.ring = vec![0.0; len];
                    self.echo.rd = 0;
                    self.echo.wr = 0;
                }
                if base != 0 && wr >= base && rd >= base {
                    self.echo.rd = ((rd - base) / 2) as usize % len;
                    self.echo.wr = ((wr - base) / 2) as usize % len;
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
            for (st, val) in
                [(&mut self.v3_music, &mut music), (&mut self.v3_fx, &mut fx)]
            {
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
        let gain = self.vf.gain();
        let (mut l, mut r) = (0.0f32, 0.0f32);
        let (mut chk_l, mut chk_r) = (0.0f32, 0.0f32);
        let mut echo_in = 0.0f32;
        let mut echo_in_chk = 0.0f32;
        let step_scale = 1.0 / self.mix_step;

        for v in self.voices.iter_mut() {
            if !v.on || v.dead {
                continue;
            }
            let s = sample_voice(v, mem, step_scale);
            v.chk_acc -= 1.0;
            if v.chk_acc <= 0.0 {
                v.chk_hold = s;
                v.chk_acc += self.mix_step;
            }
            if v.echo_send {
                // Echo-send voices feed the delay line INSTEAD of the
                // main mix (mono input: averaged magnitude weights).
                let w = (v.gl + v.gr.abs()) * 0.5 * gain;
                echo_in += s * w;
                echo_in_chk += v.chk_hold * w;
            } else {
                l += s * v.gl * gain;
                r += s * v.gr * gain;
                chk_l += v.chk_hold * v.gl * gain;
                chk_r += v.chk_hold * v.gr * gain;
            }
        }

        // Echo at the guest sample cadence (delay time is in guest
        // samples); the wet value holds between ticks.
        if self.echo_on && !self.echo.ring.is_empty() {
            self.echo.acc -= 1.0;
            if self.echo.acc <= 0.0 {
                self.echo.acc += self.mix_step;
                let e = &mut self.echo;
                let n = e.ring.len();
                let wet = e.g_wet * e.ring[e.rd] + e.g_in * echo_in;
                e.ring[e.wr] = echo_in + e.g_fb * e.ring[e.wr];
                e.rd = (e.rd + 1) % n;
                e.wr = (e.wr + 1) % n;
                e.wet_hold = wet;
            }
            l += self.echo.wet_hold;
            r += self.echo.wet_hold;
            chk_l += self.echo.wet_hold;
            chk_r += self.echo.wet_hold;
        }
        let _ = echo_in_chk;

        // Canon-domain check copy: commit clamps to s8.
        let sat = (
            chk_l.clamp(-1.0, 127.0 / 128.0),
            chk_r.clamp(-1.0, 127.0 / 128.0),
        );
        // v1 FIFO A carries LEFT.
        let canon_lr = (canon[0] as f32 / 128.0, canon[1] as f32 / 128.0);
        match self.vf.judge(canon_lr, sat) {
            Judgement::None | Judgement::Pass | Judgement::Fail { .. } => {}
        }

        if self.stereo {
            (l * 0.25, r * 0.25)
        } else {
            let m = (l + r) * 0.5;
            (m * 0.25, m * 0.25)
        }
    }
}

/// Advance and fetch one voice sample (s8/128 units) with linear
/// interpolation; handles forward loops and the backward (ping-pong)
/// leg by reflection. Per-chunk resync bounds any semantic drift to
/// one chunk.
fn sample_voice(v: &mut Voice, mem: &MemView, step_scale: f64) -> f32 {
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
    v.pos += v.dir * v.step * step_scale;
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
        let mem = MemView { rom: &rom, ewram: &ewram, iwram: &iwram };
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
            let s = sample_voice(&mut v, &mem, 1.0) * 128.0;
            assert!((0.0..16.0).contains(&s), "sample {s}");
        }
        assert!(!v.dead);
        // One-shot: dies at the end.
        let mut v2 = Voice { loop_ptr: 0, loop_len: 0, ..v };
        v2.pos = 0x0800_000Eu32 as f64;
        v2.end = 0x0800_0010u32 as f64;
        for _ in 0..4 {
            sample_voice(&mut v2, &mem, 1.0);
        }
        assert!(v2.dead);
    }
}
