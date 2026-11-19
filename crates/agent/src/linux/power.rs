//! A laptop battery through `/sys/class/power_supply`: level, state, power and energy (the
//! driver gives µWh / µW, or µAh / µA with the voltage), wear and cycles.

use super::{dirs, read, read_num, Root};
use winstats_proto::BatterySample;

/// The first system battery; None on machines without one (and for peripherals' batteries).
pub fn read_battery(root: &Root) -> Option<BatterySample> {
    let supplies = dirs(&root.path("/sys/class/power_supply"));
    let bat = supplies.iter().find(|d| read(&d.join("type")).as_deref() == Some("Battery") && read(&d.join("scope")).as_deref() != Some("Device") && read_num(&d.join("present")) != Some(0))?;
    let on_ac = supplies.iter().any(|d| read(&d.join("type")).map_or(false, |t| t == "Mains" || t == "USB") && read_num(&d.join("online")) == Some(1));
    let status = read(&bat.join("status")).unwrap_or_default();
    let n = |f: &str| read_num(&bat.join(f)).filter(|v| *v >= 0);
    let volts = n("voltage_now").map(|uv| uv as f32 / 1e6).filter(|v| *v > 0.0);
    // µWh directly, or µAh × V
    let wh = |e: &str, c: &str| n(e).map(|u| u as f32 / 1e6).or_else(|| Some(n(c)? as f32 / 1e6 * volts?));
    let remaining_wh = wh("energy_now", "charge_now");
    let full_wh = wh("energy_full", "charge_full");
    let level = n("capacity").map(|p| p as f32 / 100.0).or_else(|| Some(remaining_wh? / full_wh.filter(|f| *f > 0.0)?))?;
    let watts = n("power_now").map(|u| u as f32 / 1e6).or_else(|| Some(n("current_now")? as f32 / 1e6 * volts?));
    let charging = status == "Charging";
    let rate_w = watts.map(|w| if charging { w } else if status == "Discharging" { -w } else { 0.0 });
    let remaining_secs = match (status.as_str(), remaining_wh, watts) {
        ("Discharging", Some(e), Some(w)) if w > 0.1 => Some((e / w * 3600.0) as u32),
        _ => None,
    };
    Some(BatterySample {
        level: level.clamp(0.0, 1.0),
        charging,
        on_ac: on_ac || charging || status == "Full",
        remaining_secs,
        rate_w,
        remaining_wh,
        full_wh,
        design_wh: wh("energy_full_design", "charge_full_design"),
        cycles: n("cycle_count").filter(|c| *c > 0).map(|c| c as u32),
        voltage_mv: volts.map(|v| (v * 1000.0).round() as u32),
        // tenths of a degree, where the driver has a sensor
        temp_c: read_num(&bat.join("temp")).map(|t| t as f32 / 10.0).filter(|c| (-20.0..90.0).contains(c)),
        adapter_w: None,
        adapter_ma: None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::fake;
    use super::*;

    #[test]
    fn laptop() {
        let t = fake::tree("bat");
        let b = "sys/class/power_supply/BAT0";
        for (f, v) in [("type", "Battery"), ("present", "1"), ("status", "Discharging"), ("capacity", "80"), ("energy_now", "40000000"), ("energy_full", "50000000"), ("energy_full_design", "57000000"), ("power_now", "10000000"), ("cycle_count", "123")] {
            fake::file(&t, &format!("{b}/{f}"), v);
        }
        fake::file(&t, "sys/class/power_supply/AC/type", "Mains");
        fake::file(&t, "sys/class/power_supply/AC/online", "0");
        let s = read_battery(&Root(t)).unwrap();
        assert_eq!((s.level, s.charging, s.on_ac), (0.8, false, false));
        assert_eq!((s.rate_w, s.remaining_wh, s.full_wh, s.design_wh, s.cycles), (Some(-10.0), Some(40.0), Some(50.0), Some(57.0), Some(123)));
        assert_eq!(s.remaining_secs, Some(4 * 3600));
    }
}
