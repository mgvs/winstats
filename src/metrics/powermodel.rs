//! Passive model of the machine's own draw, learned on battery, used on AC.
//!
//!   system W  ≈  base + k · CPU package W  +  Σ over lit panels ( a_i + c_i · brightness · picture_i )
//!
//! On battery the fuel gauge measures the whole machine, so every steady stretch on battery is a
//! training sample. The CPU package (CPU + iGPU, one chip) is measured live by RAPL; `k` carries
//! the voltage converter losses. Each built-in panel has its own term: a fixed part (`a`) and a
//! part following brightness × picture level (`c`), which is what an OLED's emission does.
//!
//! Honesty rules: an estimate is offered only when every panel term it needs was seen both on
//! and off; its error is measured on samples *before* they are learned (prequential), so the
//! "±" is earned, not assumed.

use std::collections::VecDeque;

/// panels the model tells apart (a dual-screen notebook has two)
pub const SLOTS: usize = 2;
/// parameters: base, k, then (a, c) per slot
const P: usize = 2 + 2 * SLOTS;
/// steady time needed before a sample counts (the gauge averages over tens of seconds)
const STEADY_SECS: u64 = 60;
/// time after unplugging, a wake-up or a screen change before anything counts
const SETTLE_SECS: u64 = 90;
/// at most one learned point per this many seconds (neighbours carry no new information)
const LEARN_EVERY: u64 = 30;
/// samples per slot needed in each of its states (on / off) before its term is trusted
const MIN_PER_STATE: u32 = 10;
/// learned points needed before any estimate
const MIN_POINTS: u32 = 20;
/// learning-curve points kept
const ERR_HISTORY: usize = 60;

/// What the learning stands at, for showing it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Learning {
    pub n: u32,
    pub min_points: u32,
    pub min_per_state: u32,
    /// per known panel: (id, learned points lit, learned points dark)
    pub panels: Vec<(String, u32, u32)>,
    /// estimates are offered
    pub ready: bool,
    /// learned points still missing before that (at least)
    pub points_needed: u32,
    /// the shortest time on battery those points take, seconds: one settle, then one point
    /// per LEARN_EVERY while nothing changes
    pub min_secs: u32,
    /// recent prediction errors before learning (W, signed), oldest first
    pub errors: Vec<f32>,
    /// earned error of the offered estimates (W), once checked enough
    pub rms: Option<f32>,
    pub checks: u32,
}
/// slow forgetting, so the model follows ageing and new habits (half-life ≈ 7000 points)
const FORGET: f64 = 0.9999;

/// One observation of the worker.
#[derive(Clone, Debug, PartialEq)]
pub struct Obs {
    pub at: u64,
    pub on_battery: bool,
    /// the panel data describes the machine (false: Remote Desktop with the console screen on)
    pub valid: bool,
    /// machine draw measured by the battery (W), on battery only
    pub battery_w: Option<f32>,
    pub pkg_w: Option<f32>,
    /// brightness 0..1
    pub brightness: f32,
    /// per slot: None = panel dark / inactive, Some(picture level) = lit
    pub lit: [Option<Option<f32>>; SLOTS],
}

impl Obs {
    /// what must stay the same for a stretch to be steady
    fn state_key(&self) -> (bool, bool, [bool; SLOTS], u32) {
        (self.on_battery, self.valid, self.lit.map(|l| l.is_some()), (self.brightness * 20.0).round() as u32)
    }
}

/// What the learned model puts outside the chip at the current state (W): the lit screens'
/// terms, the losses that grow with the chip's draw ((factor - 1) × package), and the base.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Parts {
    pub screens: f32,
    pub conversion: f32,
    pub base: f32,
}

pub struct Model {
    /// panel id per slot, in the order first seen (upper panel first)
    pub ids: [Option<String>; SLOTS],
    xtx: [[f64; P]; P],
    xty: [f64; P],
    /// learned points
    pub n: u32,
    /// learned points per slot with the panel lit / dark
    pub on: [u32; SLOTS],
    pub off: [u32; SLOTS],
    /// prequential squared error (EMA) and how many checks fed it
    sq_err: f64,
    pub checks: u32,
    /// prediction error (W, signed) of the model as it stood, on each new point before learning
    /// it, newest last: the learning curve
    errs: VecDeque<f32>,
    window: VecDeque<Obs>,
    steady_since: u64,
    last_learn: u64,
    /// last solved parameters
    pub theta: Option<[f64; P]>,
    /// the split at the last sample, once estimates are offered
    pub last_parts: Option<Parts>,
    /// slots lit at the last sample
    lit_now: [bool; SLOTS],
}

/// prior: what an unseen machine probably does; weight = worth this many samples
const PRIOR: [f64; P] = [6.0, 1.15, 1.0, 4.0, 1.0, 4.0];
const PRIOR_W: [f64; P] = [0.05, 1.0, 0.02, 0.02, 0.02, 0.02];

impl Default for Model {
    fn default() -> Self {
        Self { ids: Default::default(), xtx: [[0.0; P]; P], xty: [0.0; P], n: 0, on: [0; SLOTS], off: [0; SLOTS], sq_err: 0.0, checks: 0, errs: VecDeque::new(), window: VecDeque::new(), steady_since: 0, last_learn: 0, theta: None, last_parts: None, lit_now: [false; SLOTS] }
    }
}

fn features(pkg: f64, brightness: f64, lit: &[Option<Option<f32>>; SLOTS]) -> Option<[f64; P]> {
    let mut x = [0.0; P];
    x[0] = 1.0;
    x[1] = pkg;
    for (i, l) in lit.iter().enumerate() {
        if let Some(apl) = l {
            x[2 + 2 * i] = 1.0;
            // a lit panel without a picture sample cannot be described
            x[3 + 2 * i] = brightness * (*apl)? as f64;
        }
    }
    Some(x)
}

/// Solve (A + diag w) θ = b + w·prior by Gaussian elimination with partial pivoting.
fn solve(a: &[[f64; P]; P], b: &[f64; P]) -> Option<[f64; P]> {
    let mut m = [[0.0; P + 1]; P];
    for i in 0..P {
        for j in 0..P {
            m[i][j] = a[i][j];
        }
        m[i][i] += PRIOR_W[i];
        m[i][P] = b[i] + PRIOR_W[i] * PRIOR[i];
    }
    for col in 0..P {
        let piv = (col..P).max_by(|&r, &s| m[r][col].abs().total_cmp(&m[s][col].abs()))?;
        if m[piv][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, piv);
        for r in 0..P {
            if r != col {
                let f = m[r][col] / m[col][col];
                for c in col..=P {
                    m[r][c] -= f * m[col][c];
                }
            }
        }
    }
    let mut th = [0.0; P];
    for i in 0..P {
        th[i] = m[i][P] / m[i][i];
    }
    th.iter().all(|v| v.is_finite()).then_some(th)
}

impl Model {
    /// Slot of a panel id, taking a free slot for a new one; None when all slots are taken.
    /// `present` are the ids of all built-in panels active right now.
    ///
    /// One physical panel can come back under another id (the device UID changes with a GPU
    /// switch or a driver update): a new id whose EDID code matches a slot whose panel is not
    /// present any more takes over that slot instead of posing as a second screen. A slot never
    /// seen lit in a learned point carries nothing and is given away when the slots run out.
    pub fn slot(&mut self, id: &str, present: &[&str]) -> Option<usize> {
        if let Some(i) = self.ids.iter().position(|s| s.as_deref() == Some(id)) {
            return Some(i);
        }
        let code = |s: &str| s.split('#').next().unwrap_or("").to_string();
        let gone = |s: &Option<String>| s.as_deref().map_or(false, |s| !present.contains(&s));
        let i = (0..SLOTS)
            .find(|&i| gone(&self.ids[i]) && self.ids[i].as_deref().map(code) == Some(code(id)))
            .or_else(|| self.ids.iter().position(|s| s.is_none()))
            .or_else(|| (0..SLOTS).find(|&i| self.on[i] == 0 && gone(&self.ids[i])))?;
        if self.ids[i].is_some() && self.ids[i].as_deref().map(code) != Some(code(id)) {
            // another panel's empty slot: its dark count belongs to that panel
            self.off[i] = 0;
        }
        self.ids[i] = Some(id.to_string());
        Some(i)
    }

    /// A panel term the estimates depend on: its panel was seen lit in a learned point, or is
    /// lit now. A slot only ever seen dark (no screen there, or an id that never came back)
    /// adds nothing to the sum and must not hold the estimates back.
    fn known(&self, i: usize, lit_now: bool) -> bool {
        self.ids[i].is_some() && (self.on[i] > 0 || lit_now)
    }

    fn mean(&self, f: impl Fn(&Obs) -> Option<f32>) -> Option<f64> {
        let v: Vec<f32> = self.window.iter().filter_map(&f).collect();
        (!v.is_empty() && v.len() * 2 >= self.window.len()).then(|| v.iter().map(|x| *x as f64).sum::<f64>() / v.len() as f64)
    }

    /// Window means as one smoothed observation (the gauge reports an average too).
    fn smoothed(&self) -> Option<(f64, [f64; P])> {
        let last = self.window.back()?;
        let pkg = self.mean(|o| o.pkg_w)?;
        let mut lit = last.lit;
        for (i, l) in lit.iter_mut().enumerate() {
            if l.is_some() {
                *l = Some(self.mean(|o| o.lit[i].flatten()).map(|v| v as f32));
            }
        }
        let x = features(pkg, last.brightness as f64, &lit)?;
        let y = self.mean(|o| o.battery_w).unwrap_or(f64::NAN);
        Some((y, x))
    }

    /// Feed one observation. Returns the estimate for it (W, ± W when earned).
    pub fn observe(&mut self, o: Obs) -> Option<(f32, Option<f32>)> {
        let key = o.state_key();
        self.lit_now = key.2;
        if self.window.back().map_or(true, |b| b.state_key() != key || o.at < b.at || o.at - b.at > 120) {
            // something changed (or a gap): a new stretch starts after the settle time
            self.window.clear();
            self.steady_since = o.at + if o.on_battery { SETTLE_SECS } else { 0 };
        }
        self.window.push_back(o.clone());
        while self.window.front().map_or(false, |f| f.at + STEADY_SECS < o.at) {
            self.window.pop_front();
        }
        let (y, x) = self.smoothed()?;
        let steady = o.at >= self.steady_since + STEADY_SECS && self.window.len() >= 3;
        let estimate = self.predict(&x, &key.2);
        self.last_parts = self.parts(&x, &key.2);
        if o.on_battery && o.valid && steady && y.is_finite() && o.at >= self.last_learn + LEARN_EVERY {
            self.last_learn = o.at;
            // the learning curve: how far the model as it stood was off, before it learns
            if let Some(th) = self.theta {
                let p: f64 = th.iter().zip(x.iter()).map(|(a, b)| a * b).sum();
                if p.is_finite() {
                    if self.errs.len() >= ERR_HISTORY {
                        self.errs.pop_front();
                    }
                    self.errs.push_back((p - y) as f32);
                }
            }
            // the error is taken before the point is learned
            if let Some((p, _)) = estimate {
                let e = p as f64 - y;
                self.sq_err = if self.checks == 0 { e * e } else { 0.95 * self.sq_err + 0.05 * e * e };
                self.checks += 1;
            }
            self.learn(&x, y, &key.2);
        }
        estimate
    }

    fn learn(&mut self, x: &[f64; P], y: f64, lit: &[bool; SLOTS]) {
        for i in 0..P {
            for j in 0..P {
                self.xtx[i][j] = self.xtx[i][j] * FORGET + x[i] * x[j];
            }
            self.xty[i] = self.xty[i] * FORGET + x[i] * y;
        }
        self.n += 1;
        for (i, on) in lit.iter().enumerate() {
            if self.ids[i].is_some() || *on {
                if *on {
                    self.on[i] += 1;
                } else {
                    self.off[i] += 1;
                }
            }
        }
        self.theta = solve(&self.xtx, &self.xty);
    }

    /// The model's figure for these features, when every term it uses has been learned.
    fn predict(&self, x: &[f64; P], lit: &[bool; SLOTS]) -> Option<(f32, Option<f32>)> {
        let th = self.theta?;
        if self.n < MIN_POINTS {
            return None;
        }
        // every known panel must have been seen lit and dark, or its term mixes with the base
        for i in 0..SLOTS {
            if (self.known(i, lit[i]) || lit[i]) && (self.on[i] < MIN_PER_STATE || self.off[i] < MIN_PER_STATE) {
                return None;
            }
        }
        let p: f64 = th.iter().zip(x.iter()).map(|(a, b)| a * b).sum();
        let err = (self.checks >= 5).then(|| (self.sq_err.sqrt() as f32).max(0.3));
        (p.is_finite() && p > 0.0).then_some((p as f32, err))
    }

    /// The terms of the model at features `x`, only where estimates are offered.
    fn parts(&self, x: &[f64; P], lit: &[bool; SLOTS]) -> Option<Parts> {
        self.predict(x, lit)?;
        let th = self.theta?;
        let screens: f64 = (0..SLOTS).map(|i| th[2 + 2 * i] * x[2 + 2 * i] + th[3 + 2 * i] * x[3 + 2 * i]).sum();
        Some(Parts { screens: screens.max(0.0) as f32, conversion: ((th[1] - 1.0) * x[1]).max(0.0) as f32, base: th[0].max(0.0) as f32 })
    }

    /// The learning state for the popup.
    pub fn learning(&self) -> Learning {
        let panels: Vec<(String, u32, u32)> = (0..SLOTS).filter(|&i| self.known(i, self.lit_now[i])).filter_map(|i| self.ids[i].clone().map(|id| (id, self.on[i], self.off[i]))).collect();
        // points every known panel still needs lit and dark, and in total
        let deficits: u32 = panels.iter().map(|(_, on, off)| MIN_PER_STATE.saturating_sub(*on) + MIN_PER_STATE.saturating_sub(*off)).sum();
        let points_needed = MIN_POINTS.saturating_sub(self.n).max(deficits);
        let min_secs = if points_needed == 0 { 0 } else { (SETTLE_SECS + STEADY_SECS + LEARN_EVERY * (points_needed as u64 - 1)) as u32 };
        Learning {
            n: self.n,
            min_points: MIN_POINTS,
            min_per_state: MIN_PER_STATE,
            ready: points_needed == 0 && self.theta.is_some(),
            points_needed,
            min_secs,
            errors: self.errs.iter().cloned().collect(),
            rms: (self.checks >= 5).then(|| (self.sq_err.sqrt() as f32).max(0.3)),
            checks: self.checks,
            panels,
        }
    }

    /// Readiness in words for the log: "ready", "learning 12/20", "panel 1: 3 on / 40 off".
    pub fn status(&self) -> String {
        if self.n < MIN_POINTS {
            return format!("learning {}/{}", self.n, MIN_POINTS);
        }
        for i in 0..SLOTS {
            if self.known(i, self.lit_now[i]) && (self.on[i] < MIN_PER_STATE || self.off[i] < MIN_PER_STATE) {
                return format!("panel {i}: {} on / {} off", self.on[i], self.off[i]);
            }
        }
        "ready".into()
    }

    // ------------------------------------------------------------ persistence (plain text)

    pub fn to_text(&self) -> String {
        let mut s = String::from("v1\n");
        for id in &self.ids {
            s.push_str(&format!("id {}\n", id.as_deref().unwrap_or("-")));
        }
        s.push_str(&format!("n {} {} {}\n", self.n, self.checks, self.sq_err));
        s.push_str(&format!("on {}\noff {}\n", self.on.map(|v| v.to_string()).join(" "), self.off.map(|v| v.to_string()).join(" ")));
        for row in &self.xtx {
            s.push_str(&format!("xtx {}\n", row.map(|v| format!("{v:e}")).join(" ")));
        }
        s.push_str(&format!("xty {}\n", self.xty.map(|v| format!("{v:e}")).join(" ")));
        if !self.errs.is_empty() {
            s.push_str(&format!("errs {}\n", self.errs.iter().map(|e| format!("{e:.3}")).collect::<Vec<_>>().join(" ")));
        }
        if let Some(th) = self.theta {
            s.push_str(&format!("# theta base={:.3} k={:.4} a0={:.3} c0={:.3} a1={:.3} c1={:.3}\n", th[0], th[1], th[2], th[3], th[4], th[5]));
        }
        s
    }

    pub fn from_text(text: &str) -> Option<Self> {
        let mut m = Model::default();
        let mut lines = text.lines();
        if lines.next()? != "v1" {
            return None;
        }
        let (mut slot, mut row) = (0, 0);
        for l in lines {
            let mut it = l.split_whitespace();
            let nums = |it: std::str::SplitWhitespace| it.map(|v| v.parse::<f64>().ok()).collect::<Option<Vec<f64>>>();
            match it.next() {
                Some("id") if slot < SLOTS => {
                    let v = it.next()?;
                    m.ids[slot] = (v != "-").then(|| v.to_string());
                    slot += 1;
                }
                Some("n") => {
                    let v = nums(it)?;
                    m.n = *v.first()? as u32;
                    m.checks = *v.get(1)? as u32;
                    m.sq_err = *v.get(2)?;
                }
                Some("on") => nums(it)?.iter().take(SLOTS).enumerate().for_each(|(i, v)| m.on[i] = *v as u32),
                Some("off") => nums(it)?.iter().take(SLOTS).enumerate().for_each(|(i, v)| m.off[i] = *v as u32),
                Some("xtx") if row < P => {
                    let v = nums(it)?;
                    (v.len() == P).then_some(())?;
                    m.xtx[row].copy_from_slice(&v);
                    row += 1;
                }
                Some("errs") => m.errs = nums(it)?.iter().map(|v| *v as f32).collect(),
                Some("xty") => {
                    let v = nums(it)?;
                    (v.len() == P).then_some(())?;
                    m.xty.copy_from_slice(&v);
                }
                _ => {}
            }
        }
        if m.n > 0 {
            m.theta = solve(&m.xtx, &m.xty);
        }
        Some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic machine: base 5 W, converter factor 1.12, panel 0 = 1 + 9·b·apl,
    /// panel 1 = 0.8 + 7·b·apl.
    fn truth(pkg: f32, b: f32, lit: &[Option<Option<f32>>; SLOTS]) -> f32 {
        let mut w = 5.0 + 1.12 * pkg;
        if let Some(Some(a)) = lit[0] {
            w += 1.0 + 9.0 * b * a;
        }
        if let Some(Some(a)) = lit[1] {
            w += 0.8 + 7.0 * b * a;
        }
        w
    }

    /// Steady stretches of 5 min each, sampled every 10 s, cycling through states.
    fn run(m: &mut Model, states: &[(f32, f32, [Option<Option<f32>>; SLOTS])], reps: usize) -> u64 {
        let mut at = 0;
        for r in 0..reps {
            for (pkg, b, lit) in states {
                let pkg = pkg + (r % 3) as f32 * 2.0;
                for _ in 0..30 {
                    at += 10;
                    m.observe(Obs { at, on_battery: true, valid: true, battery_w: Some(truth(pkg, *b, lit)), pkg_w: Some(pkg), brightness: *b, lit: *lit });
                }
            }
        }
        at
    }

    #[test]
    fn learns_each_panel_state_separately() {
        let mut m = Model::default();
        m.slot("TOP", &[]);
        m.slot("BOTTOM", &[]);
        let states = [
            (4.0, 0.9, [None, None]),
            (12.0, 0.9, [Some(Some(0.2)), None]),
            (8.0, 0.5, [Some(Some(0.6)), None]),
            (6.0, 0.9, [None, Some(Some(0.4))]),
            (20.0, 0.7, [Some(Some(0.3)), Some(Some(0.7))]),
            (3.0, 0.3, [Some(Some(0.9)), Some(Some(0.1))]),
        ];
        let at = run(&mut m, &states, 8);
        assert_eq!(m.status(), "ready");
        // every state, including ones with a picture level never seen, on AC
        for (pkg, b, lit) in [(10.0, 0.9, [None, None]), (10.0, 0.9, [Some(Some(0.5)), None]), (10.0, 0.6, [None, Some(Some(0.5))]), (15.0, 0.8, [Some(Some(0.4)), Some(Some(0.4))])] {
            let mut ac = Model::from_text(&m.to_text()).unwrap();
            let mut est = None;
            for i in 0..10 {
                est = ac.observe(Obs { at: at + 1000 + i * 10, on_battery: false, valid: true, battery_w: None, pkg_w: Some(pkg), brightness: b, lit });
            }
            let (w, _) = est.expect("estimate");
            let t = truth(pkg, b, &lit);
            assert!((w - t).abs() < 0.3, "{lit:?}: {w} vs {t}");
        }
        let err = m.observe(Obs { at: at + 10, on_battery: true, valid: true, battery_w: Some(10.0), pkg_w: Some(4.0), brightness: 0.9, lit: [None, None] }).unwrap().1;
        assert!(err.unwrap() < 0.5, "noise-free data must give a small prequential error");
    }

    #[test]
    fn no_estimate_for_a_panel_never_seen_dark() {
        let mut m = Model::default();
        m.slot("TOP", &[]);
        // the top panel always lit while learning: its term cannot be told from the base
        let states = [(4.0, 0.9, [Some(Some(0.3)), None]), (14.0, 0.5, [Some(Some(0.6)), None])];
        let at = run(&mut m, &states, 6);
        assert!(m.status().starts_with("panel 0"));
        let est = m.observe(Obs { at: at + 10, on_battery: false, valid: true, battery_w: None, pkg_w: Some(5.0), brightness: 0.9, lit: [None, None] });
        assert_eq!(est, None);
    }

    #[test]
    fn remote_desktop_with_screens_on_is_not_learned() {
        let mut m = Model::default();
        let states = [(4.0, 0.9, [None, None])];
        let mut at = 0;
        for _ in 0..200 {
            at += 10;
            m.observe(Obs { at, on_battery: true, valid: false, battery_w: Some(9.0), pkg_w: Some(4.0), brightness: 0.9, lit: states[0].2 });
        }
        assert_eq!(m.n, 0);
    }

    #[test]
    fn settle_time_after_a_change_is_skipped() {
        let mut m = Model::default();
        for i in 1..=14 {
            // 140 s: settle 90 + steady 60 not reached yet
            m.observe(Obs { at: i * 10, on_battery: true, valid: true, battery_w: Some(9.0), pkg_w: Some(4.0), brightness: 0.9, lit: [None, None] });
        }
        assert_eq!(m.n, 0);
        m.observe(Obs { at: 160, on_battery: true, valid: true, battery_w: Some(9.0), pkg_w: Some(4.0), brightness: 0.9, lit: [None, None] });
        assert_eq!(m.n, 1);
    }

    #[test]
    fn learning_curve_and_needs_are_reported() {
        let mut m = Model::default();
        m.slot("TOP", &[]);
        let l = m.learning();
        assert_eq!((l.n, l.ready, l.points_needed), (0, false, 20));
        // 20 points still needed: one settle + steady stretch, then 19 more at 30 s each
        assert_eq!(l.min_secs, 90 + 60 + 30 * 19);
        let states = [(4.0, 0.9, [None, None]), (12.0, 0.7, [Some(Some(0.4)), None])];
        run(&mut m, &states, 6);
        let l = m.learning();
        assert!(l.ready, "{l:?}");
        assert_eq!(l.points_needed, 0);
        assert!(!l.errors.is_empty());
        // noise-free data: the curve ends near zero
        assert!(l.errors.last().unwrap().abs() < 0.5, "{:?}", l.errors);
        let back = Model::from_text(&m.to_text()).unwrap();
        assert_eq!(back.learning().errors.len(), l.errors.len());
    }

    #[test]
    fn panel_deficits_count_towards_the_need() {
        let mut m = Model::default();
        m.slot("TOP", &[]);
        // only lit samples: 20 points learned, but the panel was never seen dark
        run(&mut m, &[(4.0, 0.9, [Some(Some(0.3)), None]), (9.0, 0.9, [Some(Some(0.5)), None])], 6);
        let l = m.learning();
        assert!(l.n >= 20);
        assert!(!l.ready);
        assert_eq!(l.points_needed, 10, "{l:?}");
    }

    #[test]
    fn a_slot_never_seen_lit_holds_nothing_back() {
        // one screen, plus a slot that was opened once but never lit in a learned point
        let mut m = Model::default();
        m.slot("SDC1#UID1", &[]);
        m.slot("BOE9#UID5", &[]);
        let states = [(4.0, 0.9, [None, None]), (12.0, 0.7, [Some(Some(0.4)), None])];
        let at = run(&mut m, &states, 6);
        let l = m.learning();
        assert!(l.ready, "{l:?}");
        assert_eq!(l.panels.len(), 1, "only the real screen is shown: {l:?}");
        assert_eq!(m.status(), "ready");
        let est = m.observe(Obs { at: at + 10, on_battery: false, valid: true, battery_w: None, pkg_w: Some(5.0), brightness: 0.9, lit: [Some(Some(0.4)), None] });
        assert!(est.is_some());
    }

    #[test]
    fn a_panel_under_a_new_id_keeps_its_slot() {
        let mut m = Model::default();
        assert_eq!(m.slot("ABC1234#UID1", &["ABC1234#UID1"]), Some(0));
        m.on[0] = 30;
        m.off[0] = 12;
        // the same panel after a GPU switch: same EDID code, another UID, the old one gone
        assert_eq!(m.slot("ABC1234#UID7", &["ABC1234#UID7"]), Some(0));
        assert_eq!(m.ids[1], None);
        assert_eq!((m.on[0], m.off[0]), (30, 12));
    }

    #[test]
    fn two_panels_of_one_make_get_two_slots() {
        // a dual-screen notebook: both panels share the EDID code, both are present
        let mut m = Model::default();
        let both = ["ABC1234#UID1", "ABC1234#UID2"];
        assert_eq!(m.slot("ABC1234#UID1", &both), Some(0));
        assert_eq!(m.slot("ABC1234#UID2", &both), Some(1));
    }

    #[test]
    fn a_never_lit_slot_is_given_to_a_new_panel() {
        let mut m = Model::default();
        m.slot("AAA1#UID1", &[]);
        m.slot("BBB2#UID2", &[]);
        m.on[0] = 20;
        m.off[1] = 40;
        // slots full; slot 1 never lit and its panel gone: a new panel takes it, fresh
        assert_eq!(m.slot("CCC3#UID3", &["AAA1#UID1", "CCC3#UID3"]), Some(1));
        assert_eq!(m.off[1], 0);
    }

    #[test]
    fn model_survives_a_restart() {
        let mut m = Model::default();
        m.slot("A", &[]);
        run(&mut m, &[(4.0, 0.9, [None, None]), (9.0, 0.9, [Some(Some(0.5)), None])], 6);
        let back = Model::from_text(&m.to_text()).unwrap();
        assert_eq!(back.ids, m.ids);
        assert_eq!(back.n, m.n);
        assert_eq!(back.on, m.on);
        let (a, b) = (back.theta.unwrap(), m.theta.unwrap());
        assert!(a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-6));
    }
}
