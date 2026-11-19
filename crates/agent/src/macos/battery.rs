//! The internal battery through IOPowerSources, what the menu bar battery icon shows.

use super::{boolean, get, int, string, CFArrayGetCount, CFArrayGetValueAtIndex, CFTypeRef, Cf, Io};
use winstats_proto::BatterySample;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFTypeRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFTypeRef;
    fn IOPSCopyExternalPowerAdapterDetails() -> CFTypeRef;
}

pub fn read() -> Option<BatterySample> {
    let blob = Cf::new(unsafe { IOPSCopyPowerSourcesInfo() })?;
    let list = Cf::new(unsafe { IOPSCopyPowerSourcesList(blob.0) })?;
    for i in 0..unsafe { CFArrayGetCount(list.0) } {
        let d = unsafe { IOPSGetPowerSourceDescription(blob.0, CFArrayGetValueAtIndex(list.0, i)) };
        if string(get(d, "Type")).as_deref() != Some("InternalBattery") || boolean(get(d, "Is Present")) == Some(false) {
            continue;
        }
        let cur = int(get(d, "Current Capacity"))? as f32;
        let max = int(get(d, "Max Capacity")).unwrap_or(100).max(1) as f32;
        let on_ac = string(get(d, "Power Source State")).as_deref() == Some("AC Power");
        let charging = boolean(get(d, "Is Charging")).unwrap_or(false);
        // time to empty as on Windows: minutes on battery, -1 while macOS is still estimating
        let remaining_secs = if on_ac { None } else { int(get(d, "Time to Empty")).filter(|m| *m > 0).map(|m| m as u32 * 60) };
        let mut b = BatterySample { level: (cur / max).clamp(0.0, 1.0), charging, on_ac, remaining_secs, ..Default::default() };
        smart(&mut b);
        if on_ac {
            adapter(&mut b);
        }
        return Some(b);
    }
    None
}

/// Power, energy and wear from the battery's own controller (AppleSmartBattery): mAh and mV,
/// turned into Wh and W.
fn smart(b: &mut BatterySample) {
    let Some(dev) = Io::services("AppleSmartBattery").into_iter().next() else { return };
    let data = dev.prop("BatteryData");
    let num = |k: &str| -> Option<i64> {
        dev.prop(k).and_then(|v| int(v.0)).or_else(|| data.as_ref().and_then(|d| int(get(d.0, k))))
    };
    let Some(mv) = num("Voltage").filter(|v| *v > 0) else { return };
    let v = mv as f32 / 1000.0;
    let wh = |mah: Option<i64>| mah.filter(|m| *m > 0).map(|m| m as f32 * v / 1000.0);
    b.remaining_wh = wh(num("AppleRawCurrentCapacity").or_else(|| num("RemainingCapacity")));
    b.full_wh = wh(num("AppleRawMaxCapacity").or_else(|| num("FullChargeCapacity")));
    b.design_wh = wh(num("DesignCapacity"));
    b.cycles = num("CycleCount").filter(|c| *c >= 0).map(|c| c as u32);
    b.voltage_mv = Some(mv as u32);
    // hundredths of a degree; a plausible pack temperature only
    b.temp_c = num("Temperature").map(|t| t as f32 / 100.0).filter(|c| (-20.0..90.0).contains(c));
    // mA, signed: negative while discharging (a 64-bit wrap on some models)
    let ma = num("InstantAmperage").filter(|a| *a != 0).or_else(|| num("Amperage")).map(|a| a as i32);
    b.rate_w = ma.map(|a| a as f32 * v / 1000.0);
}

/// The charger plugged in, as macOS describes it: its rating and the current it allows.
fn adapter(b: &mut BatterySample) {
    let Some(d) = Cf::new(unsafe { IOPSCopyExternalPowerAdapterDetails() }) else { return };
    b.adapter_w = int(get(d.0, "Watts")).filter(|w| *w > 0 && *w < 1000).map(|w| w as u32);
    b.adapter_ma = int(get(d.0, "Current")).filter(|a| *a > 0 && *a < 20_000).map(|a| a as u32);
}
