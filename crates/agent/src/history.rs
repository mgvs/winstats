//! The battery's history, kept by the agent itself so a master that was off (or just started)
//! still gets the past week: a point every `STEP` seconds (charge, battery power, the machine's
//! draw), in memory and in `battery_history.csv` next to the config, so a restart or a reboot of
//! the agent keeps it too.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use winstats_proto::{BatteryPoint, Snapshot};

/// seconds between points, on the same grid as winstats' own battery history
pub const STEP: u64 = 30;
/// a week of points
const KEEP_SECS: u64 = 7 * 24 * 3600;
const HEADER: &str = "at,level,on_ac,batt_w,sys_w,cpu_w\n";

pub struct BatteryHistory {
    points: VecDeque<BatteryPoint>,
    path: Option<PathBuf>,
    /// lines in the file: it is rewritten without the old ones once it holds twice the week
    lines: usize,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn num(v: Option<f32>) -> String {
    v.filter(|v| v.is_finite()).map(|v| format!("{v:.3}")).unwrap_or_default()
}

fn line(p: &BatteryPoint) -> String {
    format!("{},{:.4},{},{},{},{}\n", p.at, p.level, p.on_ac as u8, num(p.batt_w), num(p.sys_w), num(p.cpu_w))
}

fn parse(l: &str) -> Option<BatteryPoint> {
    let f: Vec<&str> = l.trim().split(',').collect();
    if f.len() < 6 {
        return None;
    }
    let opt = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite());
    Some(BatteryPoint { at: f[0].parse().ok()?, level: f[1].parse().ok()?, on_ac: f[2] == "1", batt_w: opt(f[3]), sys_w: opt(f[4]), cpu_w: opt(f[5]) })
}

impl BatteryHistory {
    /// The history in `path` (None: memory only), the last week of it.
    pub fn load(path: Option<PathBuf>) -> Self {
        let oldest = now_unix().saturating_sub(KEEP_SECS);
        let mut points: VecDeque<BatteryPoint> = VecDeque::new();
        let mut lines = 0;
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            for l in text.lines().skip(1) {
                lines += 1;
                if let Some(p) = parse(l).filter(|p| p.at >= oldest && (0.0..=1.0).contains(&p.level)) {
                    if points.back().map_or(true, |b| p.at > b.at) {
                        points.push_back(p);
                    }
                }
            }
        }
        BatteryHistory { points, path, lines }
    }

    /// A point from this sample when the grid says one is due; it is kept and returned (to go
    /// out with the sample).
    pub fn record(&mut self, s: &Snapshot) -> Option<BatteryPoint> {
        let b = s.battery.as_ref()?;
        let now = now_unix();
        let at = now - now % STEP;
        if self.points.back().map_or(false, |p| p.at >= at) {
            return None;
        }
        let watts = |id: &str| s.power.iter().find(|p| p.id == id).map(|p| p.w).filter(|w| w.is_finite() && (0.0..=1000.0).contains(w));
        let p = BatteryPoint {
            at,
            level: b.level.clamp(0.0, 1.0),
            on_ac: b.on_ac,
            batt_w: b.rate_w.filter(|w| w.is_finite() && w.abs() <= 1000.0),
            sys_w: watts("System"),
            cpu_w: watts("CPU"),
        };
        self.points.push_back(p.clone());
        let oldest = at.saturating_sub(KEEP_SECS);
        while self.points.front().map_or(false, |p| p.at < oldest) {
            self.points.pop_front();
        }
        self.save(&p);
        Some(p)
    }

    /// Append the point; rewrite the file with the kept week once the old lines have piled up.
    fn save(&mut self, p: &BatteryPoint) {
        let Some(path) = &self.path else { return };
        if self.lines > self.points.len() * 2 + 100 || !path.exists() {
            let mut text = String::from(HEADER);
            for q in &self.points {
                text.push_str(&line(q));
            }
            if std::fs::write(path, text).is_ok() {
                self.lines = self.points.len();
            }
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
            if f.write_all(line(p).as_bytes()).is_ok() {
                self.lines += 1;
            }
        }
    }

    /// The points after `at` (0: all of them), oldest first.
    pub fn since(&self, at: u64) -> Vec<BatteryPoint> {
        self.points.iter().filter(|p| p.at > at).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winstats_proto::{BatterySample, PowerSample};

    #[test]
    fn keeps_points_on_the_grid_and_reads_them_back() {
        let dir = std::env::temp_dir().join(format!("winstats-hist-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("battery_history.csv");
        let _ = std::fs::remove_file(&path);
        let mut h = BatteryHistory::load(Some(path.clone()));
        let snap = Snapshot { battery: Some(BatterySample { level: 0.8, on_ac: true, rate_w: Some(12.5), ..Default::default() }), power: vec![PowerSample { id: "System".into(), w: 21.0 }], ..Default::default() };
        let p = h.record(&snap).expect("a first point");
        assert_eq!(p.at % STEP, 0);
        assert_eq!((p.batt_w, p.sys_w, p.cpu_w, p.on_ac), (Some(12.5), Some(21.0), None, true));
        // the same grid slot gives no second point
        assert!(h.record(&snap).is_none());
        // a glitch is not kept as a reading
        let bad = Snapshot { battery: snap.battery.clone(), power: vec![PowerSample { id: "System".into(), w: 12_760.0 }], ..Default::default() };
        h.points.clear();
        assert_eq!(h.record(&bad).unwrap().sys_w, None);
        // back from the file
        let again = BatteryHistory::load(Some(path.clone()));
        assert_eq!(again.since(0).last().map(|p| p.at), Some(p.at));
        assert!(again.since(p.at).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
