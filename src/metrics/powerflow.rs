//! Where the power goes: adapter -> battery + system, or battery -> system.
//!
//! The rules keep every number honest:
//! - The battery rate comes from the fuel gauge: measured.
//! - On battery the machine runs from the battery alone, so the system draw *is* the discharge
//!   rate (the same measurement, not a second one; it is counted once).
//! - On AC the system draw is measured only when an energy meter has a platform rail. The
//!   adapter input is then battery + system (calculated). Without that rail both stay unknown:
//!   nothing is made up from the adapter's rating or past readings.

use super::power::Telemetry;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    /// read from a sensor
    Measured(f32),
    /// derived from measured values (sum, difference, rate / voltage)
    Calculated(f32),
    /// the hardware does not report it
    Unavailable,
}

impl Reading {
    pub fn value(self) -> Option<f32> {
        match self {
            Reading::Measured(v) | Reading::Calculated(v) => Some(v),
            Reading::Unavailable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// on AC, battery charging
    Charging,
    /// on AC, battery neither charging nor discharging (full, or held by a charge limit)
    AcIdle,
    /// on AC but the battery still discharges (adapter too weak for the load)
    AcDischarging,
    /// on battery
    Battery,
    /// AC state unknown
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Flow {
    pub mode: Mode,
    /// adapter input, W (>= 0)
    pub adapter: Reading,
    /// battery power, W: positive into the battery, negative out of it
    pub battery: Reading,
    /// what the machine itself draws, W (>= 0)
    pub system: Reading,
}

/// below this the battery counts as idle (gauges report a few hundred mW of noise)
const IDLE_W: f32 = 0.25;

pub fn flow(t: &Telemetry) -> Flow {
    let rate = t.rate_mw().map(|r| r as f32 / 1000.0);
    let platform = t.platform_watts();
    let battery = rate.map_or(Reading::Unavailable, Reading::Measured);
    let mode = match (t.on_ac, rate) {
        (Some(false), _) => Mode::Battery,
        (Some(true), Some(r)) if r > IDLE_W => Mode::Charging,
        (Some(true), Some(r)) if r < -IDLE_W => Mode::AcDischarging,
        (Some(true), Some(_)) => Mode::AcIdle,
        (Some(true), None) if t.charging() => Mode::Charging,
        (Some(true), None) => Mode::AcIdle,
        (None, _) => Mode::Unknown,
    };
    match mode {
        Mode::Battery => {
            // the battery feeds the machine alone: the discharge rate is the system draw; a
            // platform meter, where present, is the more direct reading of the same thing
            let system = match (platform, rate) {
                (Some(p), _) => Reading::Measured(p),
                (None, Some(r)) => Reading::Measured((-r).max(0.0)),
                (None, None) => Reading::Unavailable,
            };
            Flow { mode, adapter: Reading::Measured(0.0), battery, system }
        }
        Mode::Charging | Mode::AcIdle | Mode::AcDischarging => {
            let system = platform.map_or(Reading::Unavailable, Reading::Measured);
            // adapter = battery in + system; while discharging on AC the battery adds to the
            // adapter instead (rate is negative then)
            let adapter = match (system.value(), rate) {
                (Some(s), Some(r)) => Reading::Calculated((s + r).max(0.0)),
                _ => Reading::Unavailable,
            };
            Flow { mode, adapter, battery, system }
        }
        Mode::Unknown => Flow { mode, adapter: Reading::Unavailable, battery, system: platform.map_or(Reading::Unavailable, Reading::Measured) },
    }
}

/// Seconds until full while charging: (full - now) / rate. A rough figure (charging slows down
/// near the top), so it is shown as an estimate.
pub fn time_to_full(t: &Telemetry) -> Option<u32> {
    let rate = t.rate_mw()?;
    if rate <= 0 {
        return None;
    }
    let left = t.full_mwh()?.saturating_sub(t.capacity_mwh()?);
    if left == 0 {
        return None;
    }
    Some((left as u64 * 3600 / rate as u64).min(48 * 3600) as u32)
}

#[cfg(test)]
mod tests {
    use super::super::power::{Cell, MeterChannel, MeterRole};
    use super::*;

    #[test]
    fn a_draw_no_laptop_can_have_is_no_reading() {
        // on AC without a platform meter the total is the model's estimate: a runaway one is
        // left out rather than charted
        let mut t = tel(Some(true), Some(0), None);
        t.estimate = Some((12_760.0, None));
        assert!(split(&t).total.is_nan());
        t.estimate = Some((25.0, None));
        assert_eq!(split(&t).total, 25.0);
        // a measured platform draw above the charger's rating is a glitch too
        let mut t = tel(Some(true), Some(0), Some(400.0));
        t.source_w = Some(140);
        assert!(split(&t).total.is_nan());
        t.meters[0].watts = 30.0;
        assert_eq!(split(&t).total, 30.0);
    }

    fn tel(on_ac: Option<bool>, rate_mw: Option<i32>, platform: Option<f32>) -> Telemetry {
        let meters = platform.map(|w| vec![MeterChannel { name: "RAPL_Package0_PSYS".into(), watts: w, role: MeterRole::Platform }]).unwrap_or_default();
        Telemetry {
            on_ac,
            cells: vec![Cell { rate_mw, voltage_mv: Some(12_000), capacity_mwh: Some(40_000), full_mwh: Some(50_000), charging: rate_mw.map_or(false, |r| r > 0), ..Default::default() }],
            meters,
            ..Default::default()
        }
    }

    #[test]
    fn on_battery_the_discharge_is_the_system_draw_counted_once() {
        let f = flow(&tel(Some(false), Some(-14_600), None));
        assert_eq!(f.mode, Mode::Battery);
        assert_eq!(f.battery, Reading::Measured(-14.6));
        assert_eq!(f.system, Reading::Measured(14.6));
        assert_eq!(f.adapter, Reading::Measured(0.0));
    }

    #[test]
    fn charging_without_a_platform_meter_leaves_system_and_adapter_unknown() {
        let f = flow(&tel(Some(true), Some(58_180), None));
        assert_eq!(f.mode, Mode::Charging);
        assert_eq!(f.battery, Reading::Measured(58.18));
        assert_eq!(f.system, Reading::Unavailable);
        assert_eq!(f.adapter, Reading::Unavailable, "no adapter figure may be invented");
    }

    #[test]
    fn charging_with_a_platform_meter_sums_to_the_adapter() {
        let f = flow(&tel(Some(true), Some(58_180), Some(14.6)));
        assert_eq!(f.system, Reading::Measured(14.6));
        match f.adapter {
            Reading::Calculated(v) => assert!((v - 72.78).abs() < 0.01),
            other => panic!("expected a calculated adapter input, got {other:?}"),
        }
    }

    #[test]
    fn weak_adapter_battery_helps_out() {
        let f = flow(&tel(Some(true), Some(-5_000), Some(70.0)));
        assert_eq!(f.mode, Mode::AcDischarging);
        assert_eq!(f.adapter.value(), Some(65.0));
    }

    #[test]
    fn idle_noise_is_not_charging() {
        assert_eq!(flow(&tel(Some(true), Some(120), None)).mode, Mode::AcIdle);
    }

    #[test]
    fn missing_rate_is_unavailable_not_zero() {
        let f = flow(&tel(Some(false), None, None));
        assert_eq!(f.battery, Reading::Unavailable);
        assert_eq!(f.system, Reading::Unavailable);
    }

    #[test]
    fn time_to_full_from_capacity_and_rate() {
        // 10 Wh left at 20 W: half an hour
        assert_eq!(time_to_full(&tel(Some(true), Some(20_000), None)), Some(1800));
        assert_eq!(time_to_full(&tel(Some(false), Some(-20_000), None)), None);
    }
}

/// The machine's draw split by consumer, as far as it is known; NaN where unknown.
/// `parts`: [cores (or the whole package where the split is missing), graphics, rest of the
/// chip incl. memory, screens, conversion losses, everything else]. The chip parts are
/// measured (RAPL); screens and conversion come from the learned model; "everything else" is
/// the total minus the rest, so it exists only where the total is known.
#[derive(Clone, Copy, Debug)]
pub struct Split {
    /// what the machine drew: measured, calculated or the model's estimate on AC
    pub total: f32,
    pub parts: [f32; 6],
}

impl Split {
    #[allow(dead_code)]
    pub const NONE: Split = Split { total: f32::NAN, parts: [f32::NAN; 6] };
}

/// The most a battery machine can draw, W: a little over its charger's rating where it reports
/// one, else what the largest laptop chargers give. A measured or estimated draw above it is a
/// glitch or a model gone astray, not a reading: it is left out of the split and the history.
pub fn ceiling(t: &Telemetry) -> f32 {
    match t.source_w.filter(|w| *w >= 5) {
        Some(w) => w as f32 * 1.25,
        None => 330.0,
    }
}

pub fn split(t: &Telemetry) -> Split {
    use super::power::MeterRole;
    let meters = if t.meters_avg.is_empty() { &t.meters } else { &t.meters_avg };
    let named = |suffix: &str| meters.iter().find(|m| m.name.ends_with(suffix)).map(|m| m.watts);
    let package = meters.iter().find(|m| m.role == MeterRole::CpuPackage).map(|m| m.watts);
    let dram = named("_DRAM").unwrap_or(0.0).max(0.0);
    let sane = |v: Option<f32>| v.filter(|w| *w >= 0.0 && package.map_or(true, |p| *w <= p * 1.15));
    let (cores, graphics) = (sane(named("_PP0")), sane(named("_PP1")));
    let mut parts = [f32::NAN; 6];
    let chip = package.map(|p| p + dram);
    match (package, cores) {
        (Some(p), Some(c)) => {
            let g = graphics.unwrap_or(0.0);
            parts[0] = c;
            if let Some(g) = graphics {
                parts[1] = g;
            }
            parts[2] = (p - c - g).max(0.0) + dram;
        }
        (Some(p), None) => parts[0] = p + dram,
        _ => {}
    }
    let total = match flow(t).system.value() {
        Some(v) => Some(v),
        None => (t.on_ac == Some(true)).then_some(t.estimate).flatten().map(|e| e.0).filter(|w| *w >= chip.unwrap_or(0.0)),
    };
    let Some(total) = total.filter(|v| v.is_finite() && *v >= 0.0 && *v <= ceiling(t)) else {
        return Split { total: f32::NAN, parts };
    };
    let rest = (total - chip.unwrap_or(0.0)).max(0.0);
    let mut other = rest;
    if let Some(pp) = t.parts.filter(|pp| pp.screens + pp.conversion > 0.05 && rest > 0.0) {
        // as on the diagram: the learned parts never take more than 95 % of the remainder
        let k = ((rest * 0.95) / (pp.screens + pp.conversion)).min(1.0);
        parts[3] = pp.screens * k;
        parts[4] = pp.conversion * k;
        other = (rest - parts[3] - parts[4]).max(0.0);
    }
    parts[5] = other;
    Split { total, parts }
}
