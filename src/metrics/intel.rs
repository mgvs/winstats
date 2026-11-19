//! Intel extras from the Graphics Control Library (ControlLib.dll, installed with the Arc and
//! Xe drivers): power from the energy counters, fan, temperature where D3DKMT has none. Layouts
//! follow igcl_api.h (ctl_power_telemetry_t version 1). Older drivers (UHD 6xx and earlier) do
//! not ship the library; then nothing is added.

use super::gpuperf::Perf;
use windows::Win32::Foundation::HMODULE;
use windows::core::PCSTR;

type Handle = *mut core::ffi::c_void;

#[repr(C)]
#[derive(Default)]
struct InitArgs {
    size: u32,
    version: u8,
    app_version: u32,
    flags: u32,
    supported_version: u32,
    app_uid: [u8; 16],
}

/// ctl_device_adapter_properties_t
#[repr(C)]
struct AdapterProps {
    size: u32,
    version: u8,
    device_id: *mut core::ffi::c_void,
    device_id_size: u32,
    device_type: u32,
    supported_subfunction_flags: u32,
    driver_version: u64,
    firmware_version: [u64; 3],
    pci_vendor_id: u32,
    pci_device_id: u32,
    rev_id: u32,
    num_eus_per_sub_slice: u32,
    num_sub_slices_per_slice: u32,
    num_slices: u32,
    name: [u8; 100],
    graphics_adapter_properties: u32,
    frequency: u32,
    pci_subsys_id: u16,
    pci_subsys_vendor_id: u16,
    adapter_bdf: [u8; 3],
    num_xe_cores: u32,
    reserved: [u8; 108],
}

/// ctl_oc_telemetry_item_t
#[repr(C)]
#[derive(Clone, Copy)]
struct Item {
    supported: bool,
    units: u32,
    kind: u32,
    value: u64,
}

/// ctl_psu_info_t
#[repr(C)]
#[derive(Clone, Copy)]
struct Psu {
    supported: bool,
    psu_type: u32,
    energy: Item,
    voltage: Item,
}

/// ctl_power_telemetry_t, version 1
#[repr(C)]
struct Telemetry {
    size: u32,
    version: u8,
    time_stamp: Item,
    gpu_energy: Item,
    gpu_voltage: Item,
    gpu_clock: Item,
    gpu_temp: Item,
    global_activity: Item,
    render_activity: Item,
    media_activity: Item,
    gpu_limited: [bool; 5],
    vram_energy: Item,
    vram_voltage: Item,
    vram_clock: Item,
    vram_effective: Item,
    vram_read_counter: Item,
    vram_write_counter: Item,
    vram_temp: Item,
    vram_limited: [bool; 5],
    card_energy: Item,
    psu: [Psu; 5],
    fan: [Item; 5],
    gpu_vr_temp: Item,
    vram_vr_temp: Item,
    sa_vr_temp: Item,
    gpu_effective_clock: Item,
    gpu_over_voltage: Item,
    gpu_power_percent: Item,
    gpu_temp_percent: Item,
    vram_read_bw: Item,
    vram_write_bw: Item,
}

const UNITS_CELSIUS: u32 = 5;
const UNITS_JOULES: u32 = 6;
const UNITS_SECONDS: u32 = 7;
const UNITS_RPM: u32 = 9;
const UNITS_PERCENT: u32 = 11;

impl Item {
    /// The value as f64 by its data type; None when unsupported or not numeric.
    fn get(&self) -> Option<f64> {
        if !self.supported {
            return None;
        }
        let v = self.value;
        Some(match self.kind {
            0 => v as u8 as i8 as f64,
            1 => v as u8 as f64,
            2 => v as u16 as i16 as f64,
            3 => v as u16 as f64,
            4 => v as u32 as i32 as f64,
            5 => v as u32 as f64,
            6 => v as i64 as f64,
            7 => v as f64,
            8 => f32::from_bits(v as u32) as f64,
            9 => f64::from_bits(v),
            _ => return None,
        })
        .filter(|x| x.is_finite())
    }
}

type FnInit = unsafe extern "C" fn(*mut InitArgs, *mut Handle) -> i32;
type FnEnum = unsafe extern "C" fn(Handle, *mut u32, *mut Handle) -> i32;
type FnProps = unsafe extern "C" fn(Handle, *mut AdapterProps) -> i32;
type FnTelemetry = unsafe extern "C" fn(Handle, *mut Telemetry) -> i32;

pub struct Intel {
    telemetry: FnTelemetry,
    /// (adapter LUID, device handle, last (timestamp s, energy J))
    devices: Vec<(u64, Handle, Option<(f64, f64)>)>,
}

unsafe fn sym<T: Copy>(lib: HMODULE, name: &[u8]) -> Option<T> {
    let f = windows::Win32::System::LibraryLoader::GetProcAddress(lib, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&f))
}

impl Intel {
    pub fn new() -> Option<Self> {
        let lib = crate::util::load_system_dll(windows::core::w!("ControlLib.dll"))?;
        unsafe {
            let init: FnInit = sym(lib, b"ctlInit\0")?;
            let enumerate: FnEnum = sym(lib, b"ctlEnumerateDevices\0")?;
            let props: FnProps = sym(lib, b"ctlGetDeviceProperties\0")?;
            let telemetry: FnTelemetry = sym(lib, b"ctlPowerTelemetryGet\0")?;
            let mut args = InitArgs { size: std::mem::size_of::<InitArgs>() as u32, app_version: 1 << 16 | 1, flags: 1, ..Default::default() };
            let mut api: Handle = std::ptr::null_mut();
            if init(&mut args, &mut api) != 0 || api.is_null() {
                return None;
            }
            let mut n = 0u32;
            if enumerate(api, &mut n, std::ptr::null_mut()) != 0 || n == 0 {
                return None;
            }
            let mut handles = vec![std::ptr::null_mut(); n.min(16) as usize];
            n = handles.len() as u32;
            if enumerate(api, &mut n, handles.as_mut_ptr()) != 0 {
                return None;
            }
            let mut devices = Vec::new();
            for &h in handles.iter().take(n as usize) {
                let mut luid = 0u64;
                let mut p: AdapterProps = std::mem::zeroed();
                p.size = std::mem::size_of::<AdapterProps>() as u32;
                p.device_id = &mut luid as *mut u64 as *mut _;
                p.device_id_size = 8;
                // graphics adapters only (CTL_DEVICE_TYPE_GRAPHICS = 1)
                if props(h, &mut p) == 0 && p.device_type == 1 && luid != 0 {
                    devices.push((luid, h, None));
                }
            }
            if devices.is_empty() {
                return None;
            }
            Some(Self { telemetry, devices })
        }
    }

    /// Add what the telemetry knows about the adapter with this LUID.
    pub fn fill(&mut self, luid: u64, p: &mut Perf) {
        let Some(dev) = self.devices.iter_mut().find(|d| d.0 == luid) else { return };
        let mut t: Telemetry = unsafe { std::mem::zeroed() };
        t.size = std::mem::size_of::<Telemetry>() as u32;
        t.version = 1;
        if unsafe { (self.telemetry)(dev.1, &mut t) } != 0 {
            return;
        }
        let unit = |i: &Item, u: u32| if i.units == u { i.get() } else { None };
        if p.temp.is_none() {
            p.temp = unit(&t.gpu_temp, UNITS_CELSIUS).filter(|c| *c > 0.0 && *c < 150.0).map(|c| c as f32);
        }
        // board energy where the card reports it, else the GPU chip's; watts from the change
        let card = unit(&t.card_energy, UNITS_JOULES);
        let energy = card.or_else(|| unit(&t.gpu_energy, UNITS_JOULES));
        // without a board meter (integrated graphics) the counter is the graphics cores' alone:
        // measured equal to the RAPL graphics rail (PP1) on an Arc 140T, idle and at full load
        p.power_cores_only = card.is_none() && energy.is_some();
        let stamp = unit(&t.time_stamp, UNITS_SECONDS);
        if let (Some(e), Some(s)) = (energy, stamp) {
            if let Some((ps, pe)) = dev.2 {
                if s > ps && e >= pe {
                    let w = (e - pe) / (s - ps);
                    if w > 0.0 && w < 2000.0 {
                        p.power_w = Some(w as f32);
                    }
                }
            }
            dev.2 = Some((s, e));
        }
        if let Some(f) = t.fan.iter().find(|f| f.supported) {
            match f.units {
                UNITS_RPM => p.fan_rpm = f.get().filter(|r| *r > 0.0 && *r < 20_000.0).map(|r| r as u32).or(p.fan_rpm),
                UNITS_PERCENT => p.fan_pct = f.get().filter(|r| (0.0..=100.0).contains(r)).map(|r| r as u32).or(p.fan_pct),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cargo test intel_power_probe -- --ignored --nocapture`: read-only. Prints the raw IGCL
    /// energy / time stamp, the watts derived from them, and the RAPL graphics rail (PP1) and
    /// package for comparison, once a second for 10 s.
    #[test]
    #[ignore]
    fn intel_power_probe() {
        let mut intel = Intel::new().expect("IGCL");
        let mut meters = super::super::emi::Meters::open();
        let luid = intel.devices[0].0;
        let h = intel.devices[0].1;
        let secs: usize = std::env::var("PROBE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
        let mut prev: Option<(f64, f64)> = None;
        let mut prev_act: Option<(f64, f64)> = None;
        for _ in 0..=secs {
            let mut t: Telemetry = unsafe { std::mem::zeroed() };
            t.size = std::mem::size_of::<Telemetry>() as u32;
            t.version = 1;
            let _ = unsafe { (intel.telemetry)(h, &mut t) };
            let e = t.gpu_energy.get();
            let c = t.card_energy.get();
            let s = t.time_stamp.get();
            let act = t.global_activity.get();
            // activity counter: seconds busy; its rate is the utilisation
            let util = match (prev_act, act, s) {
                (Some((pa, ps)), Some(a), Some(s)) if s > ps => Some(((a - pa) / (s - ps) * 100.0).round()),
                _ => None,
            };
            if let (Some(a), Some(s)) = (act, s) {
                prev_act = Some((a, s));
            }
            let derived = match (prev, e, s) {
                (Some((pe, ps)), Some(e), Some(s)) if s > ps => Some((e - pe) / (s - ps)),
                _ => None,
            };
            if let (Some(e), Some(s)) = (e, s) {
                prev = Some((e, s));
            }
            let m = meters.as_mut().map(|m| m.sample()).unwrap_or_default();
            let w = |n: &str| m.iter().find(|c| c.name.ends_with(n)).map(|c| c.watts);
            println!(
                "{:>8.1}s busy {:>4?}% | IGCL {:?} W | RAPL PP1 {:?} W, PP0 {:?} W, PKG {:?} W  (card_energy {:?})",
                s.unwrap_or(0.0) % 10000.0, util, derived.map(|v| (v * 100.0).round() / 100.0), w("_PP1"), w("_PP0"), w("_PKG"), c
            );
            let mut p = Perf::default();
            intel.fill(luid, &mut p);
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    }
}
