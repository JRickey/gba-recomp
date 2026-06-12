//! Engine-agnostic HLE shadow verification: the differential
//! self-check (envelope correlation + level ratio vs the canon FIFO
//! stream), auto-gain calibration, and the pause-and-reprobe state
//! machine. Each engine shadow (MP2K, GAX, ...) renders voices and a
//! canon-domain check copy; this module decides whether the stream is
//! proven enough to substitute, and how to react when it stops being.

/// Differential self-check: shadow mix vs the canon FIFO DAC stream
/// (which keeps running underneath). Raw-sample correlation is the
/// wrong tool — the canon stream is an 8-bit ZOH staircase whose
/// image energy decorrelates from a clean render, and tonal content
/// flips sign within a couple ms of lag error. What we police is
/// *structure*: the same notes at the same loudness at the same time.
/// Both streams are rectified and lowpassed into envelopes, decimated,
/// and Pearson-correlated over a coarse lag range covering the
/// driver's output pipeline delay.
struct SelfCheck {
    /// One-pole envelope followers (per side, canon and shadow).
    lp_x: (f32, f32),
    lp_y: (f32, f32),
    /// Decimated envelopes for the current window.
    ex: Vec<(f32, f32)>,
    ey: Vec<(f32, f32)>,
    phase: u32,
    strikes: u32,
}

/// Envelope decimation: one entry per 64 grid samples (~1 ms).
const DECIM: u32 = 64;
/// Window: 1 s (1024 decimated entries).
const CHECK_WINDOW: usize = 1024;
/// Lag search 0..=56 decimated steps (~3.5 driver frames), step 2.
const CHECK_MAX_LAG: usize = 56;
/// Envelope follower pole (~80 Hz at the grid rate).
const ENV_ALPHA: f32 = 0.0075;
/// Windows whose canon envelope mean is below this carry no verdict.
const CHECK_MIN_LEVEL: f32 = 0.002;
/// Sustained correlation below this pauses the shadow.
const CHECK_MIN_R: f32 = 0.5;
const CHECK_MAX_STRIKES: u32 = 3;

impl SelfCheck {
    fn new() -> SelfCheck {
        SelfCheck {
            lp_x: (0.0, 0.0),
            lp_y: (0.0, 0.0),
            ex: Vec::with_capacity(CHECK_WINDOW),
            ey: Vec::with_capacity(CHECK_WINDOW),
            phase: 0,
            strikes: 0,
        }
    }

    /// Feed one grid sample; returns Some((best_r, level_ratio)) at
    /// window ends that produced a verdict.
    fn push(&mut self, canon: (f32, f32), shadow: (f32, f32)) -> Option<(f32, f32)> {
        self.lp_x.0 += ENV_ALPHA * (canon.0.abs() - self.lp_x.0);
        self.lp_x.1 += ENV_ALPHA * (canon.1.abs() - self.lp_x.1);
        self.lp_y.0 += ENV_ALPHA * (shadow.0.abs() - self.lp_y.0);
        self.lp_y.1 += ENV_ALPHA * (shadow.1.abs() - self.lp_y.1);
        self.phase += 1;
        if self.phase < DECIM {
            return None;
        }
        self.phase = 0;
        self.ex.push(self.lp_x);
        self.ey.push(self.lp_y);
        if self.ex.len() < CHECK_WINDOW {
            return None;
        }

        // Pipeline skew between canon and shadow varies by engine and
        // render model (mix-ahead + FIFO depth one way, delayed-render
        // shadows the other): search the lag both ways.
        let n = CHECK_WINDOW - 2 * CHECK_MAX_LAG;
        let mean = |v: &[(f32, f32)], side: usize| -> f64 {
            v.iter()
                .map(|e| if side == 0 { e.0 as f64 } else { e.1 as f64 })
                .sum::<f64>()
                / v.len() as f64
        };
        let canon_level = (mean(&self.ex, 0) + mean(&self.ex, 1)) / 2.0;
        let shadow_level = (mean(&self.ey, 0) + mean(&self.ey, 1)) / 2.0;
        let mut verdict: Option<(f32, f32)> = None;
        if canon_level >= CHECK_MIN_LEVEL as f64 {
            let ratio = (shadow_level / canon_level) as f32;
            // Does the canon envelope carry structure at all this
            // window (vs a held constant amplitude)?
            let canon_var: f64 = {
                let m0 = mean(&self.ex, 0);
                let m1 = mean(&self.ex, 1);
                self.ex
                    .iter()
                    .map(|e| {
                        let a = e.0 as f64 - m0;
                        let b = e.1 as f64 - m1;
                        a * a + b * b
                    })
                    .sum::<f64>()
                    / self.ex.len() as f64
            };
            if canon_var <= 1e-10 {
                // Flat canon: nothing to correlate — level is the
                // whole verdict (r reported 1, the gate is the ratio).
                verdict = Some((1.0, ratio));
            } else {
                let mut best: Option<f32> = None;
                for lag in (-(CHECK_MAX_LAG as isize)..=CHECK_MAX_LAG as isize).step_by(2) {
                    let mut sum = 0.0;
                    let mut sides = 0;
                    for side in 0..2 {
                        let x: Vec<f64> = self.ex[CHECK_MAX_LAG..CHECK_WINDOW - CHECK_MAX_LAG]
                            .iter()
                            .map(|e| if side == 0 { e.0 as f64 } else { e.1 as f64 })
                            .collect();
                        let y0 = (CHECK_MAX_LAG as isize - lag) as usize;
                        let y: Vec<f64> = self.ey[y0..y0 + n]
                            .iter()
                            .map(|e| if side == 0 { e.0 as f64 } else { e.1 as f64 })
                            .collect();
                        let mx = x.iter().sum::<f64>() / n as f64;
                        let my = y.iter().sum::<f64>() / n as f64;
                        let mut cov = 0.0;
                        let mut vx = 0.0;
                        let mut vy = 0.0;
                        for (a, b) in x.iter().zip(&y) {
                            cov += (a - mx) * (b - my);
                            vx += (a - mx) * (a - mx);
                            vy += (b - my) * (b - my);
                        }
                        if vx > 1e-12 && vy > 1e-12 {
                            sum += cov / (vx * vy).sqrt();
                            sides += 1;
                        }
                    }
                    if sides > 0 {
                        let r = (sum / sides as f64) as f32;
                        best = Some(best.map_or(r, |b: f32| b.max(r)));
                    }
                }
                // A structured canon with a flat shadow is the worst
                // verdict (silent-desync class), not an abstention.
                verdict = Some((best.unwrap_or(0.0), ratio));
            }
        }
        self.ex.clear();
        self.ey.clear();
        verdict
    }
}

/// One window's outcome, for engine-specific behavior search.
#[derive(Clone, Copy, PartialEq)]
pub enum Judgement {
    /// No verdict this sample (mid-window or canon silent).
    None,
    /// Window passed all gates.
    Pass,
    /// Window failed. `structure_at_sane_level` = correlation failed
    /// while the level was in-band — the signature of a semantics
    /// mismatch an engine may be able to search over.
    Fail { structure_at_sane_level: bool },
}

/// The engine-agnostic verification state machine.
pub struct Verifier {
    check: SelfCheck,
    /// Differentially calibrated output gain (1.0 = stock scale):
    /// driver revisions scale their mixers by constant factors; a gain
    /// is trusted only when structure already correlates strongly, and
    /// it is also what correct PCM/PSG balance requires.
    gain: f32,
    calibrated: bool,
    ratio_hist: [f32; 3],
    ratio_n: u32,
    /// Some window already passed level IN-BAND at strong structure —
    /// the stock scale is right, so an off-band stretch is content
    /// (e.g. a canon-side source the shadow doesn't cover), NOT a
    /// mixer-scale revision. A true scale revision is off-band in
    /// every window; this flag locks auto-gain out for the rest.
    saw_inband: bool,
    /// At least `prove_need` consecutive clean windows passed — the
    /// frontend substitutes only proven streams.
    pub proven: bool,
    prove_need: u32,
    consec_pass: u32,
    /// Pause-and-reprobe: a failing stream pauses substitution (the
    /// frontend crossfades back) and keeps probing; each pause
    /// escalates the re-prove requirement.
    pub pauses: u64,
    /// Set on each pause with the reason; frontends surface it.
    pub reverted: Option<String>,
    /// Most recent windowed correlation and ratio (diagnostics).
    pub last_r: f32,
    pub last_ratio: f32,
}

impl Default for Verifier {
    fn default() -> Self {
        Verifier::new()
    }
}

impl Verifier {
    pub fn new() -> Verifier {
        Verifier {
            check: SelfCheck::new(),
            gain: 1.0,
            calibrated: false,
            ratio_hist: [0.0; 3],
            ratio_n: 0,
            saw_inband: false,
            proven: false,
            prove_need: 1,
            consec_pass: 0,
            pauses: 0,
            reverted: None,
            last_r: 0.0,
            last_ratio: 0.0,
        }
    }

    /// Calibrated output gain (1.0 = stock scale). Engines multiply
    /// their voice sum by this in BOTH the output and the check copy.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn last(&self) -> (f32, f32) {
        (self.last_r, self.last_ratio)
    }

    fn pause(&mut self, reason: String) {
        self.proven = false;
        self.consec_pass = 0;
        self.check.strikes = 0;
        self.prove_need = (self.prove_need * 2).min(16);
        self.pauses += 1;
        self.reverted = Some(reason);
    }

    /// Feed one grid sample of (canon, shadow-check-copy) pairs, both
    /// in the canon's s8/128 domain. Drives calibration, proving,
    /// strikes, and pauses; returns the window outcome for
    /// engine-specific searches.
    pub fn judge(&mut self, canon: (f32, f32), chk: (f32, f32)) -> Judgement {
        let Some((r, ratio)) = self.check.push(canon, chk) else {
            return Judgement::None;
        };
        self.last_r = r;
        self.last_ratio = ratio;
        if !self.calibrated && r >= 0.7 && (0.85..=1.15).contains(&ratio) {
            self.saw_inband = true;
        }
        // Probation auto-calibration: strong structure with a stable
        // off-band level means this driver revision scales its mixer
        // by a constant — measure it from the canon stream and adopt
        // it instead of striking. Never after an in-band window: a
        // scale revision is off-band everywhere, so mixed in/off-band
        // verdicts mean extra canon content, and adopting its level
        // would play everything hot (it did, on a streamed backing
        // layer the shadow doesn't cover).
        if !self.calibrated
            && !self.saw_inband
            && r >= 0.7
            && !(0.85..=1.15).contains(&ratio)
            && (0.2..=5.0).contains(&ratio)
            && self.ratio_n < 6
        {
            self.ratio_hist[(self.ratio_n % 3) as usize] = ratio;
            self.ratio_n += 1;
            if self.ratio_n >= 3 {
                let m = (self.ratio_hist[0] + self.ratio_hist[1] + self.ratio_hist[2]) / 3.0;
                if self.ratio_hist.iter().all(|x| (x / m - 1.0).abs() < 0.1) {
                    self.gain = (self.gain / m).clamp(0.25, 4.0);
                    self.calibrated = true;
                    self.check.strikes = 0;
                }
            }
            return Judgement::None;
        }
        // Two failure axes: structure (correlation) and level (the
        // envelope ratio — scale-invariant correlation alone would
        // bless wrong volume math). Legitimate ratio spread is narrow:
        // canon truncation and ZOH-image energy move it ~+-30%.
        let level_ok = (0.55..=1.6).contains(&ratio);
        let window_ok = r >= CHECK_MIN_R && level_ok;
        if self.proven {
            if window_ok {
                self.check.strikes = self.check.strikes.saturating_sub(1);
            } else {
                self.check.strikes += 1;
                if self.check.strikes >= CHECK_MAX_STRIKES {
                    self.pause(format!(
                        "shadow/canon correlation {r:.2}, level ratio {ratio:.2} \
                         (driver variant or unsupported feature)"
                    ));
                }
            }
        } else if window_ok {
            self.consec_pass += 1;
            if self.consec_pass >= self.prove_need {
                self.proven = true;
                self.check.strikes = 0;
            }
        } else {
            self.consec_pass = 0;
        }
        if window_ok {
            Judgement::Pass
        } else {
            Judgement::Fail {
                structure_at_sane_level: r < CHECK_MIN_R && level_ok,
            }
        }
    }
}
