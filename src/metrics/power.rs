//! Battery and power telemetry, read on a worker thread so a slow ACPI fuel gauge never stalls
//! the UI thread.
//!
//! Sources, in order of preference; whatever a machine does not answer stays `None`:
//! - the battery class driver (`IOCTL_BATTERY_QUERY_STATUS` / `_INFORMATION` on every
//!   `GUID_DEVCLASS_BATTERY` interface): voltage, signed rate (mW), capacities, cycle count,
//!   temperature, chemistry, charging source. No elevation is needed.
//! - `GetSystemPowerStatus`: AC line, percentage, remaining time, energy saver.
//! - Energy Meter Interface (`GUID_DEVICE_ENERGY_METER`, see `emi`): a platform / PSYS channel
//!   gives the whole machine's draw; CPU package channels are reported on their own.
//!
//! Nothing here is invented: values derived from other readings (current = rate / voltage,
//! time to full) are marked as calculated by the consumer, see `powerflow`.

use crate::util::{pcwstr, wide};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

/// {72631E54-78A4-11D0-BCF7-00AA00B7B32A}
pub const GUID_DEVCLASS_BATTERY: GUID = GUID::from_u128(0x72631e54_78a4_11d0_bcf7_00aa00b7b32a);

// battery class IOCTLs: CTL_CODE(FILE_DEVICE_BATTERY = 0x29, fn, METHOD_BUFFERED, FILE_READ_ACCESS)
const IOCTL_BATTERY_QUERY_TAG: u32 = 0x0029_4040;
const IOCTL_BATTERY_QUERY_INFORMATION: u32 = 0x0029_4044;
const IOCTL_BATTERY_QUERY_STATUS: u32 = 0x0029_404C;

// BATTERY_QUERY_INFORMATION_LEVEL
const LEVEL_INFORMATION: i32 = 0;
const LEVEL_TEMPERATURE: i32 = 2;
const LEVEL_DEVICE_NAME: i32 = 4;
const LEVEL_MANUFACTURE_NAME: i32 = 6;
const LEVEL_CHARGING_SOURCE: i32 = 9;

const BATTERY_CAPACITY_RELATIVE: u32 = 0x4000_0000;
const BATTERY_POWER_ON_LINE: u32 = 0x1;
const BATTERY_DISCHARGING: u32 = 0x2;
const BATTERY_CHARGING: u32 = 0x4;
const UNKNOWN_U32: u32 = 0xFFFF_FFFF;
const UNKNOWN_RATE: i32 = 0x8000_0000u32 as i32;

#[repr(C)]
#[derive(Default)]
struct BatteryQueryInformation {
    tag: u32,
    level: i32,
    at_rate: i32,
}

#[repr(C)]
#[derive(Default)]
struct BatteryInformation {
    capabilities: u32,
    technology: u8,
    reserved: [u8; 3],
    chemistry: [u8; 4],
    designed_capacity: u32,
    full_charged_capacity: u32,
    default_alert1: u32,
    default_alert2: u32,
    critical_bias: u32,
    cycle_count: u32,
}

#[repr(C)]
#[derive(Default)]
struct BatteryWaitStatus {
    tag: u32,
    timeout: u32,
    power_state: u32,
    low_capacity: u32,
    high_capacity: u32,
}

#[repr(C)]
#[derive(Default)]
struct BatteryStatus {
    power_state: u32,
    capacity: u32,
    voltage: u32,
    rate: i32,
}

#[repr(C)]
#[derive(Default)]
struct BatteryChargingSource {
    kind: i32,
    max_current: u32,
}

/// Where a charging source comes from (BATTERY_CHARGING_SOURCE_TYPE).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Ac,
    Usb,
    Wireless,
}

/// One physical battery as the class driver reports it.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub name: String,
    pub manufacturer: String,
    /// "LION", "LiP", "PbAc"...
    pub chemistry: String,
    /// capacities are relative units, not mWh: no watts can be derived
    pub relative: bool,
    pub capacity_mwh: Option<u32>,
    pub full_mwh: Option<u32>,
    pub design_mwh: Option<u32>,
    pub voltage_mv: Option<u32>,
    /// mW, positive while charging, negative while discharging (fuel gauge reading)
    pub rate_mw: Option<i32>,
    pub cycles: Option<u32>,
    pub temp_c: Option<f32>,
    pub charging: bool,
    pub discharging: bool,
    pub on_line: bool,
}

/// One Energy Meter Interface channel with its average power over the last sample window.
#[derive(Clone, Debug)]
pub struct MeterChannel {
    pub name: String,
    pub watts: f32,
    pub role: MeterRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeterRole {
    /// whole platform (Intel PSYS, a "SYS" rail): the machine's draw
    Platform,
    /// CPU package (RAPL PKG)
    CpuPackage,
    Other,
}

/// Everything the battery popup draws, sampled together.
#[derive(Clone, Debug, Default)]
pub struct Telemetry {
    /// when the sample was taken (wall clock, for the "read at" row)
    pub at_unix: u64,
    pub cells: Vec<Cell>,
    /// 0..1 from Windows (composite battery)
    pub level: Option<f32>,
    pub on_ac: Option<bool>,
    /// Windows energy saver (battery saver) is on
    pub saver: bool,
    /// seconds of battery left, Windows' own estimate while discharging
    pub remaining_secs: Option<u32>,
    pub source: Option<(SourceKind, Option<u32>)>,
    /// the charger's rating, W, where the machine reports it (a Mac through its agent)
    pub source_w: Option<u32>,
    pub meters: Vec<MeterChannel>,
    /// the same channels averaged over `METER_AVG_SECS`, to set against the gauge's rate
    pub meters_avg: Vec<MeterChannel>,
    /// the machine's draw as last measured on battery: (W, unix seconds). A past measurement,
    /// shown as such while on AC; never used as a live value.
    pub last_on_battery: Option<(f32, u64)>,
    /// the machine's draw on AC from the learned model (W, ± W once earned); see `powermodel`
    pub estimate: Option<(f32, Option<f32>)>,
    /// where the passive model's learning stands (machines with a battery and an energy meter)
    pub learning: Option<super::powermodel::Learning>,
    /// the learned split of what lies outside the chip (screens, conversion, base)
    pub parts: Option<super::powermodel::Parts>,
}

// ---------------------------------------------------------------- passive model + log

fn model_path() -> std::path::PathBuf {
    crate::util::config_dir().join("power_model.txt")
}

fn log_path() -> std::path::PathBuf {
    crate::util::config_dir().join("power_log.csv")
}

const LOG_HEADER: &str = "unix,on_ac,remote,console,valid,level,rate_w,batt_w,pkg_w,pp0_w,pp1_w,dram_w,platform_w,brightness,external,panels,lit0,lit1,est_w,est_err_w,status,n,pkg_avg_w\n";
/// the log rolls over to power_log.1.csv at this size
const LOG_MAX: u64 = 20 * 1024 * 1024;

/// Appends one row per sample, for checking the model later.
/// Tests start the real worker: they must not write into the user's settings folder.
const PERSIST: bool = !cfg!(test);

fn save_model(m: &super::powermodel::Model) {
    if PERSIST {
        let _ = std::fs::write(model_path(), m.to_text());
    }
}

fn log_row(t: &Telemetry, ds: &super::displays::DisplayState, valid: bool, lit: &[Option<Option<f32>>; super::powermodel::SLOTS], batt_w: Option<f32>, est: Option<(f32, Option<f32>)>, model: &super::powermodel::Model) {
    if !PERSIST {
        return;
    }
    use std::io::Write;
    let path = log_path();
    if std::fs::metadata(&path).map_or(false, |m| m.len() > LOG_MAX) {
        let _ = std::fs::rename(&path, crate::util::config_dir().join("power_log.1.csv"));
    }
    // a log written with other columns is set aside (checked once per run)
    static CHECKED: std::sync::Once = std::sync::Once::new();
    CHECKED.call_once(|| {
        let first = std::fs::read_to_string(&path).ok().and_then(|t| t.lines().next().map(str::to_string));
        if first.map_or(false, |l| l != LOG_HEADER.trim_end()) {
            let _ = std::fs::rename(&path, crate::util::config_dir().join("power_log.v1.csv"));
        }
    });
    let new = !path.exists();
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    if new {
        let _ = f.write_all(LOG_HEADER.as_bytes());
    }
    let o = |v: Option<f32>| v.map(|v| format!("{v:.3}")).unwrap_or_default();
    let meter = |name: &str| o(t.meters.iter().find(|m| m.name.ends_with(name)).map(|m| m.watts));
    let pkg = t.meters.iter().find(|m| m.role == MeterRole::CpuPackage).map(|m| m.watts);
    let panels: Vec<String> = ds.panels.iter().map(|p| format!("{}@{};{}:{}", p.id, p.x, p.y, o(p.apl))).collect();
    let litf = |l: Option<Option<f32>>| match l {
        None => "off".to_string(),
        Some(a) => o(a),
    };
    let row = format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},\"{}\",{},{},{},{},{},{},{}\n",
        t.at_unix,
        t.on_ac.map_or("", |a| if a { "1" } else { "0" }),
        ds.remote as u8,
        ds.console,
        valid as u8,
        o(t.level),
        o(t.rate_mw().map(|r| r as f32 / 1000.0)),
        o(batt_w),
        o(pkg),
        meter("_PP0"),
        meter("_PP1"),
        meter("_DRAM"),
        o(t.platform_watts()),
        ds.brightness.map(|b| b.to_string()).unwrap_or_default(),
        ds.external,
        panels.join(" "),
        litf(lit[0]),
        litf(lit[1]),
        o(est.map(|e| e.0)),
        o(est.and_then(|e| e.1)),
        model.status(),
        model.n,
        o(t.meters_avg.iter().find(|m| m.role == MeterRole::CpuPackage).map(|m| m.watts)),
    );
    let _ = f.write_all(row.as_bytes());
}

/// Feed the model one sample (and log it); the estimate it gives, if any.
fn model_step(model: &mut super::powermodel::Model, t: &Telemetry) -> Option<(f32, Option<f32>)> {
    use super::powermodel::{Obs, SLOTS};
    let on_battery = t.on_ac == Some(false);
    // the picture is sampled only on lit built-in panels (a cheap 32 x 20 copy)
    let ds = super::displays::read(t.on_ac != Some(false), true);
    // over Remote Desktop the panels are unknown, unless the console screen is off: then they
    // are all dark whatever the layout
    // a panel whose name could not be read cannot be told from another: such a sample is not
    // learned (it would open a slot for a screen that does not exist)
    let valid = (!ds.remote || ds.console == 0) && ds.panels.iter().all(|p| !p.id.is_empty());
    let mut lit: [Option<Option<f32>>; SLOTS] = [None; SLOTS];
    if valid && !ds.remote {
        let present: Vec<&str> = ds.panels.iter().map(|p| p.id.as_str()).collect();
        for p in ds.lit() {
            if let Some(i) = model.slot(&p.id, &present) {
                lit[i] = Some(p.apl);
            }
        }
    }
    let batt_w = if on_battery { t.platform_watts().or_else(|| t.rate_mw().filter(|r| *r < -250).map(|r| -r as f32 / 1000.0)) } else { None };
    let pkg_w = t.meters.iter().find(|m| m.role == MeterRole::CpuPackage).map(|m| m.watts);
    let obs = Obs { at: t.at_unix, on_battery, valid, battery_w: batt_w, pkg_w, brightness: ds.brightness.unwrap_or(100) as f32 / 100.0, lit };
    let est = model.observe(obs);
    log_row(t, &ds, valid, &lit, batt_w, est, model);
    est
}

/// The battery gauge reports an averaged rate; set against it, the energy meters must be
/// averaged over a similar time. 30 s comes from this machine's own log: the correlation of
/// the gauge's discharge rate with the trailing mean of the CPU package power peaks at 20-45 s
/// (0.47 against 0.23 for the instant value), and the gauge updates every 10 s.
pub const METER_AVG_SECS: f64 = 30.0;

/// Time-weighted trailing mean of the meter channels. Each sample is already the average over
/// its own interval (energy difference / time), so it is weighted by that interval's overlap
/// with the window: fast and slow sampling mix correctly.
#[derive(Default)]
pub struct MeterAverage {
    /// (end of the interval in seconds, its length, the channels)
    samples: std::collections::VecDeque<(f64, f64, Vec<MeterChannel>)>,
}

impl MeterAverage {
    pub fn push(&mut self, now: f64, dt: f64, meters: &[MeterChannel]) -> Vec<MeterChannel> {
        // a long gap is sleep: nothing before it belongs to the window
        if dt > 120.0 {
            self.samples.clear();
        }
        if !meters.is_empty() && dt > 0.0 {
            self.samples.push_back((now, dt, meters.to_vec()));
        }
        let from = now - METER_AVG_SECS;
        while self.samples.front().map_or(false, |(end, _, _)| *end <= from) {
            self.samples.pop_front();
        }
        let mut out: Vec<MeterChannel> = Vec::new();
        for (end, dt, channels) in &self.samples {
            let weight = (end - from).min(*dt);
            for c in channels {
                match out.iter_mut().find(|o| o.name == c.name) {
                    // accumulate watt·seconds in `watts`, seconds in the scratch below
                    Some(o) => o.watts += c.watts * weight as f32,
                    None => out.push(MeterChannel { name: c.name.clone(), watts: c.watts * weight as f32, role: c.role }),
                }
            }
        }
        for o in out.iter_mut() {
            let secs: f64 = self.samples.iter().filter(|(_, _, ch)| ch.iter().any(|c| c.name == o.name)).map(|(end, dt, _)| (end - from).min(*dt)).sum();
            o.watts = if secs > 0.0 { (o.watts as f64 / secs) as f32 } else { 0.0 };
        }
        out
    }
}

/// Keeps the system draw measured while on battery, for showing it (as a past reading) after
/// the machine is plugged in again.
#[derive(Default)]
pub struct BatteryReference {
    /// (unix seconds, W) of the current on-battery stretch
    samples: std::collections::VecDeque<(u64, f32)>,
    /// when the machine went on battery
    since: Option<u64>,
    /// time of the previous sample
    seen: Option<u64>,
    pub last: Option<(f32, u64)>,
}

/// gauges lag after unplugging or waking (a woken gauge reports the sleep drain for a while):
/// readings this young are not used
const SETTLE_SECS: u64 = 45;
/// a gap longer than this between samples is sleep: a new stretch starts
const GAP_SECS: u64 = 120;
/// an awake laptop draws more than this; lower readings are a gauge still settling
const MIN_DRAW_MW: i32 = 1500;
/// samples and time span a reference needs
const REF_MIN_SAMPLES: usize = 3;
const REF_MIN_SPAN: u64 = 30;
/// the reference is the mean over this much of the latest on-battery time
const REF_WINDOW_SECS: u64 = 60;

impl BatteryReference {
    pub fn update(&mut self, t: &Telemetry) {
        let now = t.at_unix;
        if t.on_ac != Some(false) {
            self.since = None;
            self.samples.clear();
            return;
        }
        // a long gap is sleep: start over as if just unplugged
        if self.seen.map_or(false, |s| now > s + GAP_SECS) {
            self.since = None;
            self.samples.clear();
        }
        self.seen = Some(now);
        let since = *self.since.get_or_insert(now);
        // the platform meter where there is one, else the discharge rate
        let draw = t.platform_watts().filter(|w| *w * 1000.0 >= MIN_DRAW_MW as f32).or_else(|| t.rate_mw().filter(|r| *r <= -MIN_DRAW_MW).map(|r| -r as f32 / 1000.0));
        let Some(w) = draw else { return };
        if now < since + SETTLE_SECS {
            return;
        }
        self.samples.push_back((now, w));
        while self.samples.front().map_or(false, |(at, _)| *at + REF_WINDOW_SECS < now) {
            self.samples.pop_front();
        }
        let span = self.samples.back().map_or(0, |b| b.0) - self.samples.front().map_or(0, |f| f.0);
        if self.samples.len() >= REF_MIN_SAMPLES && span >= REF_MIN_SPAN {
            let mean = self.samples.iter().map(|(_, w)| w).sum::<f32>() / self.samples.len() as f32;
            self.last = Some((mean, now));
        }
    }
}

fn reference_path() -> std::path::PathBuf {
    crate::util::config_dir().join("last_on_battery.txt")
}

fn load_reference() -> Option<(f32, u64)> {
    let s = std::fs::read_to_string(reference_path()).ok()?;
    let (at, w) = s.trim().split_once(',')?;
    let (at, w) = (at.parse::<u64>().ok()?, w.parse::<f32>().ok()?);
    (w.is_finite() && w * 1000.0 >= MIN_DRAW_MW as f32).then_some((w, at))
}

fn save_reference(r: Option<(f32, u64)>) {
    if !PERSIST {
        return;
    }
    if let Some((w, at)) = r {
        let _ = std::fs::write(reference_path(), format!("{at},{w:.3}\n"));
    }
}

impl Telemetry {
    /// Sum of the cells' rates (mW), when every cell reports one.
    pub fn rate_mw(&self) -> Option<i32> {
        if self.cells.is_empty() || self.cells.iter().any(|c| c.rate_mw.is_none() || c.relative) {
            return None;
        }
        Some(self.cells.iter().filter_map(|c| c.rate_mw).sum())
    }

    fn sum(&self, f: impl Fn(&Cell) -> Option<u32>) -> Option<u32> {
        if self.cells.is_empty() || self.cells.iter().any(|c| c.relative) {
            return None;
        }
        self.cells.iter().map(&f).sum::<Option<u32>>()
    }

    pub fn capacity_mwh(&self) -> Option<u32> {
        self.sum(|c| c.capacity_mwh)
    }
    pub fn full_mwh(&self) -> Option<u32> {
        self.sum(|c| c.full_mwh)
    }
    pub fn design_mwh(&self) -> Option<u32> {
        self.sum(|c| c.design_mwh)
    }

    /// Mean terminal voltage of the cells that report one, mV.
    pub fn voltage_mv(&self) -> Option<u32> {
        let v: Vec<u32> = self.cells.iter().filter_map(|c| c.voltage_mv).collect();
        if v.is_empty() {
            None
        } else {
            Some(v.iter().sum::<u32>() / v.len() as u32)
        }
    }

    /// Battery current in mA, derived from rate / voltage per cell (calculated, not measured).
    pub fn current_ma(&self) -> Option<i32> {
        if self.cells.is_empty() {
            return None;
        }
        let mut sum = 0i64;
        for c in &self.cells {
            let (r, v) = (c.rate_mw?, c.voltage_mv.filter(|v| *v > 0)?);
            sum += r as i64 * 1000 / v as i64;
        }
        Some(sum as i32)
    }

    /// Highest cycle count among the cells.
    pub fn cycles(&self) -> Option<u32> {
        self.cells.iter().filter_map(|c| c.cycles).max()
    }

    pub fn temp_c(&self) -> Option<f32> {
        self.cells.iter().filter_map(|c| c.temp_c).reduce(f32::max)
    }

    pub fn charging(&self) -> bool {
        self.cells.iter().any(|c| c.charging)
    }

    /// Full-charge capacity against design capacity, 0..1+.
    pub fn health(&self) -> Option<f32> {
        let (f, d) = (self.full_mwh()?, self.design_mwh()?);
        (d > 0).then(|| f as f32 / d as f32)
    }

    pub fn platform_watts(&self) -> Option<f32> {
        self.meters.iter().find(|m| m.role == MeterRole::Platform).map(|m| m.watts)
    }
}

// ---------------------------------------------------------------- battery class driver

/// Device interface paths of a class, from the configuration manager.
pub(crate) fn interface_paths(class: &GUID) -> Vec<String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CR_SUCCESS};
    let mut len = 0u32;
    unsafe {
        if CM_Get_Device_Interface_List_SizeW(&mut len, class, windows::core::PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS || len <= 1 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(class, windows::core::PCWSTR::null(), &mut buf, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS {
            return Vec::new();
        }
        buf.split(|c| *c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

pub(crate) fn open_device(path: &str, write: bool) -> Option<HANDLE> {
    // GENERIC_READ | GENERIC_WRITE: the battery IOCTLs are defined with FILE_READ_ACCESS, but
    // some miniports insist on write access as well; retry read-only
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    let p = wide(path);
    let access = if write { GENERIC_READ | GENERIC_WRITE } else { GENERIC_READ };
    unsafe { CreateFileW(pcwstr(&p), access, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None).ok() }
}

/// DeviceIoControl with plain structs in and out; the bytes returned on success.
pub(crate) fn ioctl<I, O>(h: HANDLE, code: u32, input: Option<&I>, out: &mut O) -> Option<u32> {
    ioctl_raw(h, code, input, out as *mut O as *mut u8, std::mem::size_of::<O>())
}

pub(crate) fn ioctl_raw<I>(h: HANDLE, code: u32, input: Option<&I>, out: *mut u8, out_len: usize) -> Option<u32> {
    let mut got = 0u32;
    let (ip, il) = match input {
        Some(i) => (Some(i as *const I as *const std::ffi::c_void), std::mem::size_of::<I>() as u32),
        None => (None, 0),
    };
    unsafe { DeviceIoControl(h, code, ip, il, Some(out as *mut _), out_len as u32, Some(&mut got), None).ok().map(|_| got) }
}

struct Device {
    path: String,
    handle: HANDLE,
    tag: u32,
    /// static parts read once per tag
    info: Option<BatteryInformation>,
    name: String,
    manufacturer: String,
    source_supported: bool,
    temp_supported: bool,
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn query_string(h: HANDLE, tag: u32, level: i32) -> String {
    let q = BatteryQueryInformation { tag, level, at_rate: 0 };
    let mut buf = [0u16; 128];
    match ioctl_raw(h, IOCTL_BATTERY_QUERY_INFORMATION, Some(&q), buf.as_mut_ptr() as *mut u8, std::mem::size_of_val(&buf)) {
        Some(n) => {
            let n = (n as usize / 2).min(buf.len());
            let end = buf[..n].iter().position(|c| *c == 0).unwrap_or(n);
            String::from_utf16_lossy(&buf[..end]).trim().to_string()
        }
        None => String::new(),
    }
}

impl Device {
    fn open(path: &str) -> Option<Self> {
        let handle = open_device(path, true).or_else(|| open_device(path, false))?;
        let mut d = Device { path: path.to_string(), handle, tag: 0, info: None, name: String::new(), manufacturer: String::new(), source_supported: true, temp_supported: true };
        d.refresh_tag()?;
        Some(d)
    }

    /// (Re)read the battery tag; it changes when a battery is swapped. None: no battery in this slot.
    fn refresh_tag(&mut self) -> Option<()> {
        let wait = 0u32;
        let mut tag = 0u32;
        ioctl(self.handle, IOCTL_BATTERY_QUERY_TAG, Some(&wait), &mut tag)?;
        if tag == 0 {
            return None;
        }
        if tag != self.tag {
            self.tag = tag;
            let q = BatteryQueryInformation { tag, level: LEVEL_INFORMATION, at_rate: 0 };
            let mut info = BatteryInformation::default();
            self.info = ioctl(self.handle, IOCTL_BATTERY_QUERY_INFORMATION, Some(&q), &mut info).map(|_| info);
            self.name = query_string(self.handle, tag, LEVEL_DEVICE_NAME);
            self.manufacturer = query_string(self.handle, tag, LEVEL_MANUFACTURE_NAME);
            self.source_supported = true;
            self.temp_supported = true;
        }
        Some(())
    }

    /// Re-read the dynamic parts (status, cycle count, temperature).
    fn sample(&mut self) -> Option<(Cell, Option<(SourceKind, Option<u32>)>)> {
        let ws = BatteryWaitStatus { tag: self.tag, ..Default::default() };
        let mut st = BatteryStatus::default();
        if ioctl(self.handle, IOCTL_BATTERY_QUERY_STATUS, Some(&ws), &mut st).is_none() {
            // a stale tag fails the query: re-read it once
            self.refresh_tag()?;
            let ws = BatteryWaitStatus { tag: self.tag, ..Default::default() };
            ioctl(self.handle, IOCTL_BATTERY_QUERY_STATUS, Some(&ws), &mut st)?;
        }
        // the cycle count and full-charge capacity drift: re-read the information block
        let q = BatteryQueryInformation { tag: self.tag, level: LEVEL_INFORMATION, at_rate: 0 };
        let mut info = BatteryInformation::default();
        if ioctl(self.handle, IOCTL_BATTERY_QUERY_INFORMATION, Some(&q), &mut info).is_some() {
            self.info = Some(info);
        }
        let mut temp_c = None;
        if self.temp_supported {
            let q = BatteryQueryInformation { tag: self.tag, level: LEVEL_TEMPERATURE, at_rate: 0 };
            let mut tenth_kelvin = 0u32;
            match ioctl(self.handle, IOCTL_BATTERY_QUERY_INFORMATION, Some(&q), &mut tenth_kelvin) {
                // 0 and absurd values mean the gauge has no sensor
                Some(_) if tenth_kelvin > 2331 && tenth_kelvin < 3731 => temp_c = Some(tenth_kelvin as f32 / 10.0 - 273.15),
                Some(_) => {}
                None => self.temp_supported = false,
            }
        }
        let mut source = None;
        if self.source_supported && st.power_state & BATTERY_POWER_ON_LINE != 0 {
            let q = BatteryQueryInformation { tag: self.tag, level: LEVEL_CHARGING_SOURCE, at_rate: 0 };
            let mut s = BatteryChargingSource::default();
            match ioctl(self.handle, IOCTL_BATTERY_QUERY_INFORMATION, Some(&q), &mut s) {
                Some(n) if n as usize >= std::mem::size_of::<i32>() => {
                    let kind = match s.kind {
                        1 => Some(SourceKind::Ac),
                        2 => Some(SourceKind::Usb),
                        3 => Some(SourceKind::Wireless),
                        _ => None,
                    };
                    let max = (n as usize >= std::mem::size_of::<BatteryChargingSource>() && s.max_current > 0 && s.max_current != UNKNOWN_U32).then_some(s.max_current);
                    source = kind.map(|k| (k, max));
                }
                Some(_) => {}
                None => self.source_supported = false,
            }
        }
        let info = self.info.as_ref();
        let relative = info.map_or(false, |i| i.capabilities & BATTERY_CAPACITY_RELATIVE != 0);
        let known = |v: u32| (v != UNKNOWN_U32 && v > 0).then_some(v);
        let chemistry = info.map(|i| String::from_utf8_lossy(&i.chemistry).trim_end_matches('\0').trim().to_string()).unwrap_or_default();
        Some((
            Cell {
                name: self.name.clone(),
                manufacturer: self.manufacturer.clone(),
                chemistry,
                relative,
                capacity_mwh: if st.capacity == UNKNOWN_U32 { None } else { Some(st.capacity) },
                full_mwh: info.and_then(|i| known(i.full_charged_capacity)),
                design_mwh: info.and_then(|i| known(i.designed_capacity)),
                voltage_mv: known(st.voltage),
                rate_mw: if st.rate == UNKNOWN_RATE { None } else { Some(st.rate) },
                // 0 is what gauges without a counter report
                cycles: info.and_then(|i| (i.cycle_count > 0).then_some(i.cycle_count)),
                temp_c,
                charging: st.power_state & BATTERY_CHARGING != 0,
                discharging: st.power_state & BATTERY_DISCHARGING != 0,
                on_line: st.power_state & BATTERY_POWER_ON_LINE != 0,
            },
            source,
        ))
    }
}

/// Windows' composite view: AC line, percentage, remaining time, energy saver.
fn system_status(t: &mut Telemetry) {
    let mut st = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut st) }.is_err() {
        return;
    }
    t.on_ac = match st.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    if st.BatteryLifePercent <= 100 {
        t.level = Some(st.BatteryLifePercent as f32 / 100.0);
    }
    if st.BatteryLifeTime != u32::MAX {
        t.remaining_secs = Some(st.BatteryLifeTime);
    }
    t.saver = st.SystemStatusFlag & 1 != 0;
}

// ---------------------------------------------------------------- worker

/// One point of the long history: unix seconds, level 0..1, signed battery power W (NaN unknown).
#[derive(Clone, Copy, Debug)]
pub struct HistPoint {
    pub at: u64,
    pub level: f32,
    pub watts: f32,
    /// the machine's draw split by consumer (see powerflow::split); NaN where unknown
    pub split: super::powerflow::Split,
}

/// seconds between history points
pub const HIST_STEP: u64 = 30;
/// points kept: 7 days (the large history view goes back that far)
pub const HIST_LEN: usize = (7 * 24 * 3600 / HIST_STEP) as usize;

#[derive(Default)]
struct Shared {
    latest: Option<Arc<Telemetry>>,
    history: Arc<Vec<HistPoint>>,
    seq: u64,
    /// popup open: sample every `fast`; otherwise every `slow`
    active: bool,
    fast: Duration,
    stop: bool,
}

pub struct PowerMonitor {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
    seen: u64,
}

/// sampling period while nobody looks (the history still fills)
const SLOW: Duration = Duration::from_secs(10);
/// ... and while energy saver is on
const SAVER: Duration = Duration::from_secs(30);

impl PowerMonitor {
    pub fn start(fast_ms: u32) -> Self {
        let shared = Arc::new((Mutex::new(Shared { fast: Duration::from_millis(fast_ms.max(500) as u64), history: Arc::new(load_history()), ..Default::default() }), Condvar::new()));
        let s2 = shared.clone();
        let thread = std::thread::Builder::new().name("power".into()).spawn(move || worker(s2)).ok();
        Self { shared, thread, seen: 0 }
    }

    /// Popup open (fast sampling) or closed; wakes the worker at once when it opens.
    pub fn set_active(&self, active: bool, fast_ms: u32) {
        let (m, cv) = &*self.shared;
        let mut s = m.lock().unwrap();
        let fast = Duration::from_millis(fast_ms.max(500) as u64);
        if s.active != active || s.fast != fast {
            s.active = active;
            s.fast = fast;
            cv.notify_all();
        }
    }

    /// The newest sample and history when they changed since the last call.
    pub fn take(&mut self) -> Option<(Arc<Telemetry>, Arc<Vec<HistPoint>>)> {
        let s = self.shared.0.lock().ok()?;
        if s.seq == self.seen {
            return None;
        }
        self.seen = s.seq;
        Some((s.latest.clone()?, s.history.clone()))
    }
}

impl Drop for PowerMonitor {
    fn drop(&mut self) {
        {
            let (m, cv) = &*self.shared;
            if let Ok(mut s) = m.lock() {
                s.stop = true;
            }
            cv.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn worker(shared: Arc<(Mutex<Shared>, Condvar)>) {
    let mut devices: Vec<Device> = Vec::new();
    let mut enumerated_at: Option<Instant> = None;
    let mut meter = super::emi::Meters::open();
    if let Some(m) = &meter {
        crate::log!("power: energy meter channels: {}", m.names().join(", "));
    }
    let mut last_hist = 0u64;
    let mut last_save = Instant::now();
    let mut reference = BatteryReference { last: load_reference(), ..Default::default() };
    // the passive model needs the CPU package meter: without it there is nothing to learn from
    let mut model = meter.as_ref().map(|_| std::fs::read_to_string(model_path()).ok().and_then(|s| super::powermodel::Model::from_text(&s)).unwrap_or_default());
    let mut model_saved = Instant::now();
    let mut was_on_battery = false;
    let mut meter_avg = MeterAverage::default();
    let started = Instant::now();
    let mut meter_at: Option<f64> = None;
    loop {
        // batteries come and go (docks, swappable packs): enumerate again now and then
        if enumerated_at.map_or(true, |t| t.elapsed() > Duration::from_secs(120)) || devices.is_empty() && enumerated_at.map_or(true, |t| t.elapsed() > Duration::from_secs(30)) {
            enumerated_at = Some(Instant::now());
            let paths = interface_paths(&GUID_DEVCLASS_BATTERY);
            devices.retain(|d| paths.contains(&d.path));
            for p in paths {
                if !devices.iter().any(|d| d.path == p) {
                    if let Some(d) = Device::open(&p) {
                        devices.push(d);
                    }
                }
            }
        }
        let mut t = Telemetry { at_unix: now_unix(), ..Default::default() };
        system_status(&mut t);
        devices.retain_mut(|d| match d.sample() {
            Some((cell, src)) => {
                if t.source.is_none() {
                    t.source = src;
                }
                t.cells.push(cell);
                true
            }
            None => false,
        });
        if let Some(m) = meter.as_mut() {
            t.meters = m.sample();
            let now = started.elapsed().as_secs_f64();
            let dt = meter_at.map_or(0.0, |a| now - a);
            meter_at = Some(now);
            t.meters_avg = meter_avg.push(now, dt, &t.meters);
            if m.failed() {
                meter = None;
            }
        }
        reference.update(&t);
        let on_battery = t.on_ac == Some(false);
        if was_on_battery && !on_battery {
            // plugged in again: keep the reading for the next start
            save_reference(reference.last);
        }
        let switched = was_on_battery != on_battery;
        was_on_battery = on_battery;
        t.last_on_battery = reference.last;
        // batteries only: the model learns from the battery's measurement
        if let Some(m) = model.as_mut().filter(|_| !t.cells.is_empty()) {
            let est = model_step(m, &t);
            t.learning = Some(m.learning());
            t.parts = m.last_parts;
            if t.on_ac == Some(true) {
                t.estimate = est;
            }
            if model_saved.elapsed() > Duration::from_secs(600) || switched {
                model_saved = Instant::now();
                save_model(m);
            }
        }
        // history on a fixed grid
        let hist_due = t.at_unix >= last_hist + HIST_STEP;
        let (m, cv) = &*shared;
        let mut s = m.lock().unwrap();
        if hist_due && (t.level.is_some() || !t.cells.is_empty()) {
            last_hist = t.at_unix - t.at_unix % HIST_STEP;
            let level = t.level.or_else(|| Some(t.capacity_mwh()? as f32 / t.full_mwh()?.max(1) as f32)).unwrap_or(f32::NAN);
            let watts = t.rate_mw().map(|r| r as f32 / 1000.0).unwrap_or(f32::NAN);
            let h = Arc::make_mut(&mut s.history);
            if h.len() >= HIST_LEN {
                h.drain(..h.len() + 1 - HIST_LEN);
            }
            h.push(HistPoint { at: last_hist, level, watts, split: super::powerflow::split(&t) });
        }
        let saver = t.saver;
        s.latest = Some(Arc::new(t));
        s.seq += 1;
        if last_save.elapsed() > Duration::from_secs(600) {
            last_save = Instant::now();
            save_history(&s.history);
        }
        let wait = if s.active { s.fast } else if saver { SAVER } else { SLOW };
        let started = Instant::now();
        let active_before = s.active;
        while !s.stop && started.elapsed() < wait {
            let left = wait - started.elapsed();
            s = cv.wait_timeout(s, left).unwrap().0;
            if s.active && !active_before {
                // the popup just opened: sample now
                break;
            }
        }
        if s.stop {
            save_history(&s.history);
            if was_on_battery {
                save_reference(reference.last);
            }
            if let Some(m) = &model {
                save_model(m);
            }
            return;
        }
    }
}

// ---------------------------------------------------------------- history persistence

fn history_path() -> std::path::PathBuf {
    crate::util::config_dir().join("battery_history.csv")
}

/// Points younger than a day from the last run, so the chart is not empty after a restart.
fn load_history() -> Vec<HistPoint> {
    let Ok(text) = std::fs::read_to_string(history_path()) else { return Vec::new() };
    let oldest = now_unix().saturating_sub(HIST_STEP * HIST_LEN as u64);
    let mut out: Vec<HistPoint> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let at = it.next()?.trim().parse::<u64>().ok()?;
            let level = it.next()?.trim().parse::<f32>().ok()?;
            let mut num = || it.next().and_then(|w| w.trim().parse::<f32>().ok()).unwrap_or(f32::NAN);
            let watts = num();
            // older files end here: the split stays unknown
            let total = num();
            let parts = [num(), num(), num(), num(), num(), num()];
            let split = super::powerflow::Split { total, parts };
            (at >= oldest && level.is_finite() && (0.0..=1.0).contains(&level)).then_some(HistPoint { at, level, watts, split })
        })
        .collect();
    out.sort_by_key(|p| p.at);
    out.dedup_by_key(|p| p.at);
    if out.len() > HIST_LEN {
        out.drain(..out.len() - HIST_LEN);
    }
    out
}

fn save_history(h: &[HistPoint]) {
    if !PERSIST {
        return;
    }
    if h.is_empty() {
        return;
    }
    let mut s = String::with_capacity(h.len() * 64);
    let num = |v: f32| if v.is_finite() { format!("{v:.2}") } else { String::new() };
    for p in h {
        s.push_str(&format!("{},{:.3},{},{}", p.at, p.level, num(p.watts), num(p.split.total)));
        for v in p.split.parts {
            s.push(',');
            s.push_str(&num(v));
        }
        s.push('\n');
    }
    let path = history_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(rate: Option<i32>, mv: Option<u32>) -> Cell {
        Cell { rate_mw: rate, voltage_mv: mv, capacity_mwh: Some(30_000), full_mwh: Some(50_000), design_mwh: Some(60_000), ..Default::default() }
    }

    #[test]
    fn current_is_rate_over_voltage_per_cell() {
        let t = Telemetry { cells: vec![cell(Some(58_180), Some(12_660))], ..Default::default() };
        assert_eq!(t.current_ma(), Some(4595));
        let t = Telemetry { cells: vec![cell(Some(-10_000), Some(10_000)), cell(Some(-5_000), Some(12_500))], ..Default::default() };
        assert_eq!(t.current_ma(), Some(-1400));
    }

    #[test]
    fn missing_reading_makes_the_sum_unknown() {
        let t = Telemetry { cells: vec![cell(Some(1000), Some(12_000)), cell(None, Some(12_000))], ..Default::default() };
        assert_eq!(t.rate_mw(), None);
        assert_eq!(t.current_ma(), None);
        let t = Telemetry { cells: vec![Cell { relative: true, ..cell(Some(1000), Some(12_000)) }], ..Default::default() };
        assert_eq!(t.rate_mw(), None, "relative capacity units carry no watts");
        assert_eq!(t.capacity_mwh(), None);
    }

    /// `cargo test live_probe -- --ignored --nocapture`: what this machine reports.
    #[test]
    #[ignore]
    fn live_probe() {
        let paths = interface_paths(&GUID_DEVCLASS_BATTERY);
        println!("battery interfaces: {paths:?}");
        let mut t = Telemetry::default();
        system_status(&mut t);
        for p in &paths {
            match Device::open(p) {
                Some(mut d) => {
                    let s = d.sample();
                    println!("{s:#?}");
                    if let Some((c, src)) = s {
                        t.cells.push(c);
                        t.source = t.source.or(src);
                    }
                }
                None => println!("{p}: open / tag failed"),
            }
        }
        match super::super::emi::Meters::open() {
            Some(mut m) => {
                println!("emi channels: {:?}", m.names());
                m.sample();
                std::thread::sleep(Duration::from_millis(1000));
                t.meters = m.sample();
            }
            None => println!("emi: none readable"),
        }
        println!("{t:#?}");
        println!("flow: {:?}", super::super::powerflow::flow(&t));
    }

    fn at(on_ac: bool, now: u64, rate: i32) -> Telemetry {
        Telemetry { at_unix: now, on_ac: Some(on_ac), cells: vec![cell(Some(rate), Some(12_000))], ..Default::default() }
    }

    #[test]
    fn reference_skips_the_settling_seconds_and_averages() {
        let mut r = BatteryReference::default();
        r.update(&at(false, 1000, -30_000)); // just unplugged: gauge still settling
        r.update(&at(false, 1030, -20_000));
        assert_eq!(r.last, None);
        r.update(&at(false, 1045, -14_000));
        r.update(&at(false, 1060, -16_000));
        assert_eq!(r.last, None, "two samples are not enough");
        r.update(&at(false, 1075, -15_000));
        assert_eq!(r.last, Some((15.0, 1075)));
        // plugged in: the reading stays, nothing new is added
        r.update(&at(true, 1090, 40_000));
        assert_eq!(r.last, Some((15.0, 1075)));
    }

    #[test]
    fn reference_window_forgets_old_load() {
        let mut r = BatteryReference::default();
        for (t, w) in [(0, -9_000), (50, -40_000), (100, -40_000), (110, -10_000), (140, -10_000), (170, -10_000)] {
            r.update(&at(false, t, w));
        }
        assert_eq!(r.last, Some((10.0, 170)));
    }

    #[test]
    fn waking_from_sleep_does_not_take_the_sleep_drain() {
        let mut r = BatteryReference::default();
        for t in (0..=120).step_by(10) {
            r.update(&at(false, t, -15_000));
        }
        let before = r.last;
        assert!(before.is_some());
        // 9 h asleep; the woken gauge reports the sleep average for a while
        r.update(&at(false, 32_520, -304));
        r.update(&at(false, 32_530, -9_000));
        assert_eq!(r.last, before, "the old reading stays until a settled one replaces it");
    }

    #[test]
    fn reference_ignores_idle_noise() {
        let mut r = BatteryReference::default();
        for t in (0..200).step_by(10) {
            r.update(&at(false, t, -300));
        }
        assert_eq!(r.last, None);
    }

    fn ch(w: f32) -> Vec<MeterChannel> {
        vec![MeterChannel { name: "RAPL_Package0_PKG".into(), watts: w, role: MeterRole::CpuPackage }]
    }

    #[test]
    fn meter_average_weights_by_interval_and_window() {
        let mut a = MeterAverage::default();
        // 20 s at 10 W, then 10 s at 40 W: the 30 s window holds both
        a.push(10.0, 10.0, &ch(10.0));
        a.push(20.0, 10.0, &ch(10.0));
        let r = a.push(30.0, 10.0, &ch(40.0));
        assert!((r[0].watts - 20.0).abs() < 1e-4, "{}", r[0].watts);
        // fast samples of 2 s count by their length, not their number
        let mut a = MeterAverage::default();
        a.push(28.0, 28.0, &ch(10.0));
        let r = a.push(30.0, 2.0, &ch(40.0));
        assert!((r[0].watts - 12.0).abs() < 1e-4, "{}", r[0].watts);
        // only the overlapping part of an old interval counts: 20 s outside the window
        let mut a = MeterAverage::default();
        a.push(40.0, 40.0, &ch(10.0));
        let r = a.push(50.0, 10.0, &ch(40.0));
        assert!((r[0].watts - 20.0).abs() < 1e-4, "{}", r[0].watts);
        // after sleep the window starts over
        let r = a.push(5000.0, 4950.0, &ch(7.0));
        assert!((r[0].watts - 7.0).abs() < 1e-4);
    }

    #[test]
    fn health_is_full_over_design() {
        let t = Telemetry { cells: vec![cell(None, None), cell(None, None)], ..Default::default() };
        assert_eq!(t.full_mwh(), Some(100_000));
        assert!((t.health().unwrap() - 50.0 / 60.0).abs() < 1e-6);
    }
}
