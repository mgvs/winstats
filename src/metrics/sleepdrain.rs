//! Battery lost during standby. Windows pauses WinStats while the PC sleeps (Modern Standby,
//! sleep or hibernate): a gap of at least `GAP_SECS` between two samples is a standby. It counts
//! when the PC was on battery before and after; the capacity (mWh) comes from the battery's fuel
//! gauge, read once it has a fresh reading after waking.

use std::path::PathBuf;

/// A pause this long between two samples is a standby.
pub const GAP_SECS: u64 = 120;
/// Standbys kept (and saved).
const KEEP: usize = 30;
/// After waking, wait this long at most for a fresh capacity reading.
const FRESH_WAIT_SECS: u64 = 45;

/// One standby on battery.
#[derive(Clone, Debug, PartialEq)]
pub struct SleepEvent {
    /// unix seconds: last sample before, first after
    pub from: u64,
    pub to: u64,
    /// battery level 0..1 before and after
    pub pct_from: f32,
    pub pct_to: f32,
    /// remaining capacity before and after, when the fuel gauge had fresh readings
    pub mwh_from: Option<u32>,
    pub mwh_to: Option<u32>,
}

impl SleepEvent {
    pub fn secs(&self) -> u64 {
        self.to.saturating_sub(self.from)
    }

    /// Level lost, in percentage points (never below 0).
    pub fn lost_pct(&self) -> f32 {
        ((self.pct_from - self.pct_to) * 100.0).max(0.0)
    }

    /// Energy lost in Wh, from the fuel gauge.
    pub fn lost_wh(&self) -> Option<f32> {
        Some((self.mwh_from? as f32 - self.mwh_to? as f32).max(0.0) / 1000.0)
    }

    /// Average draw while asleep in W (needs both capacities and at least 10 minutes).
    pub fn avg_w(&self) -> Option<f32> {
        let h = self.secs() as f32 / 3600.0;
        (h >= 1.0 / 6.0).then(|| self.lost_wh().map(|wh| wh / h)).flatten()
    }

    fn line(&self) -> String {
        let o = |v: Option<u32>| v.map_or("-".to_string(), |v| v.to_string());
        format!("{} {} {:.4} {:.4} {} {}", self.from, self.to, self.pct_from, self.pct_to, o(self.mwh_from), o(self.mwh_to))
    }

    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 6 {
            return None;
        }
        let o = |s: &str| if s == "-" { Some(None) } else { s.parse().ok().map(Some) };
        Some(Self { from: f[0].parse().ok()?, to: f[1].parse().ok()?, pct_from: f[2].parse().ok()?, pct_to: f[3].parse().ok()?, mwh_from: o(f[4])?, mwh_to: o(f[5])? })
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    unix: u64,
    pct: f32,
    mwh: Option<u32>,
    on_ac: bool,
}

/// Watches the samples for standby gaps.
#[derive(Default)]
pub struct SleepTracker {
    last: Option<Sample>,
    /// a standby seen, waiting for a fresh capacity reading: (before, woke at)
    pending: Option<(Sample, u64)>,
    pub events: Vec<SleepEvent>,
    path: Option<PathBuf>,
}

impl SleepTracker {
    /// With the saved standbys (sleep_log.txt in the settings folder).
    pub fn load() -> Self {
        let path = crate::util::config_dir().join("sleep_log.txt");
        let events = std::fs::read_to_string(&path).map(|s| s.lines().filter_map(SleepEvent::parse).collect()).unwrap_or_default();
        Self { path: Some(path), events, ..Default::default() }
    }

    /// One sample: unix seconds, level 0..1, capacity in mWh when the reading is fresh, AC.
    /// True when a standby was recorded.
    pub fn update(&mut self, now: u64, pct: f32, mwh: Option<u32>, on_ac: bool) -> bool {
        let s = Sample { unix: now, pct, mwh, on_ac };
        let mut recorded = false;
        if let Some((before, woke)) = self.pending {
            // finish once the gauge reads again (or give up waiting for it)
            if on_ac {
                self.pending = None;
            } else if mwh.is_some() || now.saturating_sub(woke) >= FRESH_WAIT_SECS {
                self.pending = None;
                recorded = self.record(SleepEvent { from: before.unix, to: woke, pct_from: before.pct, pct_to: pct, mwh_from: before.mwh, mwh_to: mwh });
            }
        }
        if let Some(prev) = self.last {
            let gap = now.saturating_sub(prev.unix);
            if gap >= GAP_SECS && !prev.on_ac && !on_ac {
                // the capacity before is the last fresh one; after waking it may still be stale
                self.pending = Some((prev, now));
            }
        }
        // keep the last sample with a fresh capacity as "before"
        self.last = Some(match (self.last, mwh) {
            (Some(p), None) if p.mwh.is_some() && now.saturating_sub(p.unix) < 30 && p.on_ac == on_ac => Sample { unix: now, pct, mwh: p.mwh, on_ac },
            _ => s,
        });
        recorded
    }

    fn record(&mut self, e: SleepEvent) -> bool {
        // a level that went up is not a standby on battery (charged meanwhile)
        if e.pct_to > e.pct_from + 0.005 {
            return false;
        }
        crate::log!("standby: {} min, -{:.1} %, {:?} Wh", e.secs() / 60, e.lost_pct(), e.lost_wh());
        self.events.push(e);
        if self.events.len() > KEEP {
            let n = self.events.len() - KEEP;
            self.events.drain(..n);
        }
        if let Some(p) = self.path.as_ref() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let text: String = self.events.iter().map(|e| e.line() + "\n").collect();
            let _ = std::fs::write(p, text);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> SleepTracker {
        SleepTracker::default()
    }

    #[test]
    fn a_gap_on_battery_is_a_standby() {
        let mut t = tracker();
        t.update(1000, 0.80, Some(57_000), false);
        t.update(1001, 0.80, Some(57_000), false);
        // 8 h later, the gauge is still stale at first
        assert!(!t.update(1001 + 8 * 3600, 0.76, None, false));
        assert!(t.update(1003 + 8 * 3600, 0.76, Some(54_000), false));
        let e = &t.events[0];
        assert_eq!(e.from, 1001);
        assert_eq!(e.secs(), 8 * 3600);
        assert!((e.lost_pct() - 4.0).abs() < 0.01);
        assert!((e.avg_w().unwrap() - 0.375).abs() < 0.001);
    }

    #[test]
    fn plugged_in_or_short_pauses_do_not_count() {
        let mut t = tracker();
        t.update(1000, 0.80, Some(57_000), true);
        t.update(9000, 0.80, Some(57_000), false);
        t.update(9010, 0.80, Some(57_000), false);
        // 60 s is a hiccup, not a standby
        t.update(9070, 0.80, Some(57_000), false);
        t.update(9071, 0.80, Some(57_000), false);
        assert!(t.events.is_empty());
        // charged while away: not a standby on battery
        t.update(20_000, 0.90, Some(64_000), false);
        t.update(20_001, 0.90, Some(64_000), false);
        assert!(t.events.is_empty());
    }

    #[test]
    fn without_a_fresh_reading_it_records_the_level_only() {
        let mut t = tracker();
        t.update(1000, 0.50, Some(36_000), false);
        t.update(5000, 0.48, None, false);
        assert!(t.update(5000 + FRESH_WAIT_SECS, 0.48, None, false));
        assert_eq!(t.events[0].mwh_to, None);
        assert_eq!(t.events[0].avg_w(), None);
    }

    #[test]
    fn lines_round_trip() {
        let e = SleepEvent { from: 1, to: 7201, pct_from: 0.8, pct_to: 0.78, mwh_from: Some(57_000), mwh_to: None };
        assert_eq!(SleepEvent::parse(&e.line()), Some(e));
    }
}
