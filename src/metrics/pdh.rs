//! Performance-counter based metrics: disk throughput and GPU utilisation
//! (the same "GPU Engine" counters Task Manager reads).

use crate::util::{pcwstr, wide};
use std::collections::HashMap;
use windows::Win32::System::Performance::*;

pub struct Pdh {
    query: isize,
    disk_read: isize,
    disk_write: isize,
    gpu: Option<isize>,
    /// per-instance "PhysicalDisk(*)" counters for the popup
    disk_read_all: Option<isize>,
    disk_write_all: Option<isize>,
    /// "Processor Information(*)": current performance in % of the base clock, and the base clock
    cpu_perf: Option<isize>,
    cpu_base: Option<isize>,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    /// Per adapter LUID: the busiest engine type in 0..1; empty when the GPU counters are
    /// unavailable (VMs, old drivers).
    pub gpu_loads: HashMap<u64, f32>,
}

impl Pdh {
    pub fn new() -> Option<Self> {
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| -> Option<isize> {
                let mut c = 0isize;
                let p = wide(path);
                if PdhAddEnglishCounterW(query, pcwstr(&p), 0, &mut c) == 0 { Some(c) } else { None }
            };
            let disk_read = add(r"\PhysicalDisk(_Total)\Disk Read Bytes/sec")?;
            let disk_write = add(r"\PhysicalDisk(_Total)\Disk Write Bytes/sec")?;
            let disk_read_all = add(r"\PhysicalDisk(*)\Disk Read Bytes/sec");
            let disk_write_all = add(r"\PhysicalDisk(*)\Disk Write Bytes/sec");
            let cpu_perf = add(r"\Processor Information(*)\% Processor Performance");
            let cpu_base = add(r"\Processor Information(*)\Processor Frequency");
            let gpu = add(r"\GPU Engine(*)\Utilization Percentage");
            if gpu.is_none() {
                crate::log!("GPU Engine counters unavailable; gpu widget will show --");
            }
            let _ = PdhCollectQueryData(query);
            Some(Self { query, disk_read, disk_write, gpu, disk_read_all, disk_write_all, cpu_perf, cpu_base, disk_read_bps: 0.0, disk_write_bps: 0.0, gpu_loads: HashMap::new() })
        }
    }

    pub fn gpu_available(&self) -> bool {
        self.gpu.is_some()
    }

    /// Formatted values of an instance array counter as (instance name, value).
    fn array(counter: isize) -> Vec<(String, f64)> {
        unsafe {
            let mut size = 0u32;
            let mut count = 0u32;
            let r = PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if r != PDH_MORE_DATA || size == 0 {
                return Vec::new();
            }
            let mut buf = vec![0u8; size as usize];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return Vec::new();
            }
            std::slice::from_raw_parts(items, count as usize)
                .iter()
                .filter(|it| it.FmtValue.CStatus == 0)
                .map(|it| (it.szName.to_string().unwrap_or_default(), it.FmtValue.Anonymous.doubleValue))
                .collect()
        }
    }

    /// Per physical disk (PhysicalDriveN index) read and write bytes/s, from instances like "0 C:".
    pub fn per_disk(&self) -> Vec<(u32, f64, f64)> {
        let (Some(r), Some(w)) = (self.disk_read_all, self.disk_write_all) else { return Vec::new() };
        let index_of = |name: &str| -> Option<u32> { name.split_whitespace().next()?.parse().ok() };
        let mut out: Vec<(u32, f64, f64)> = Vec::new();
        for (name, v) in Self::array(r) {
            if let Some(i) = index_of(&name) {
                out.push((i, v, 0.0));
            }
        }
        for (name, v) in Self::array(w) {
            if let Some(i) = index_of(&name) {
                if let Some(e) = out.iter_mut().find(|e| e.0 == i) {
                    e.2 = v;
                } else {
                    out.push((i, 0.0, v));
                }
            }
        }
        out.sort_by_key(|e| e.0);
        out
    }

    /// Current clock per logical processor in MHz (base clock x performance), the way Task
    /// Manager computes it; instances are "group,index".
    pub fn core_freqs(&self) -> Vec<u32> {
        let (Some(p), Some(b)) = (self.cpu_perf, self.cpu_base) else { return Vec::new() };
        let index_of = |name: &str| -> Option<usize> {
            let (g, i) = name.split_once(',')?;
            let g: usize = g.parse().ok()?;
            let i: usize = i.parse().ok()?;
            Some(g * 64 + i)
        };
        let mut base: Vec<(usize, f64)> = Self::array(b).into_iter().filter_map(|(n, v)| index_of(&n).map(|i| (i, v))).collect();
        let perf: Vec<(usize, f64)> = Self::array(p).into_iter().filter_map(|(n, v)| index_of(&n).map(|i| (i, v))).collect();
        base.sort_by_key(|e| e.0);
        base.iter()
            .map(|(i, mhz)| {
                let pf = perf.iter().find(|e| e.0 == *i).map(|e| e.1).unwrap_or(100.0);
                (mhz * pf / 100.0).round().max(0.0) as u32
            })
            .collect()
    }

    fn value(counter: isize) -> Option<f64> {
        let mut v = PDH_FMT_COUNTERVALUE::default();
        let r = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut v) };
        if r == 0 { Some(unsafe { v.Anonymous.doubleValue }) } else { None }
    }

    /// Task Manager style, per adapter: sum utilisation per engine type, report the busiest one.
    /// Instances look like "pid_1234_luid_0x00000000_0x0000D1C9_phys_0_eng_3_engtype_3D".
    fn gpu_values(counter: isize) -> Option<HashMap<u64, f32>> {
        unsafe {
            let mut size = 0u32;
            let mut count = 0u32;
            let r = PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if r != PDH_MORE_DATA || size == 0 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            let r = PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items));
            if r != 0 {
                return None;
            }
            let slice = std::slice::from_raw_parts(items, count as usize);
            let mut per_type: HashMap<(u64, String), f64> = HashMap::new();
            for it in slice {
                if it.FmtValue.CStatus != 0 {
                    continue;
                }
                let name = it.szName.to_string().unwrap_or_default();
                let engtype = name.rsplit("engtype_").next().unwrap_or("").to_string();
                *per_type.entry((luid_of(&name), engtype)).or_default() += it.FmtValue.Anonymous.doubleValue;
            }
            let mut out: HashMap<u64, f32> = HashMap::new();
            for ((luid, _), v) in per_type {
                let v = (v / 100.0).clamp(0.0, 1.0) as f32;
                let e = out.entry(luid).or_insert(0.0);
                *e = e.max(v);
            }
            Some(out)
        }
    }

    /// GPU load per (process id, adapter LUID), Task Manager style: the busiest engine type of
    /// that process on that adapter. Uses the last collected sample, so it costs no extra query.
    pub fn gpu_by_pid(&self) -> HashMap<(u32, u64), f32> {
        let Some(g) = self.gpu else { return HashMap::new() };
        let mut per: HashMap<(u32, u64, String), f64> = HashMap::new();
        for (name, v) in Self::array(g) {
            let Some(pid) = pid_of(&name) else { continue };
            let engtype = name.rsplit("engtype_").next().unwrap_or("").to_string();
            *per.entry((pid, luid_of(&name), engtype)).or_default() += v;
        }
        let mut out: HashMap<(u32, u64), f32> = HashMap::new();
        for ((pid, luid, _), v) in per {
            let v = (v / 100.0).clamp(0.0, 1.0) as f32;
            let e = out.entry((pid, luid)).or_insert(0.0);
            *e = e.max(v);
        }
        out
    }

    pub fn update(&mut self) {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return;
            }
        }
        if let Some(v) = Self::value(self.disk_read) {
            self.disk_read_bps = v;
        }
        if let Some(v) = Self::value(self.disk_write) {
            self.disk_write_bps = v;
        }
        if let Some(g) = self.gpu {
            if let Some(v) = Self::gpu_values(g) {
                self.gpu_loads = v;
            }
        }
    }
}

/// The process id in a GPU Engine / GPU Process Memory instance name ("pid_1234_luid_...").
fn pid_of(name: &str) -> Option<u32> {
    name.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

/// Counters only the popups show, in a query of their own that is created when a popup first
/// opens and collected only while one is open. Each counter is optional: a missing one (older
/// Windows, no GPU counters) leaves its values empty.
pub struct Details {
    query: isize,
    ctx_switches: Option<isize>,
    interrupts: Option<isize>,
    dpc_time: Option<isize>,
    interrupt_time: Option<isize>,
    perf_limit: Option<isize>,
    /// ACPI thermal zones: tenths of a kelvin (Windows 10+), whole kelvins (older)
    zone_temp_hp: Option<isize>,
    zone_temp: Option<isize>,
    disk_idle: Option<isize>,
    disk_queue: Option<isize>,
    disk_latency: Option<isize>,
    disk_iops: Option<isize>,
    gpu_proc_dedicated: Option<isize>,
    gpu_proc_shared: Option<isize>,
    gpu_adapter_dedicated: Option<isize>,
    gpu_adapter_shared: Option<isize>,
}

#[derive(Default)]
pub struct DetailSample {
    /// per second
    pub ctx_switches: Option<f64>,
    pub interrupts: Option<f64>,
    /// share of CPU time, 0..1
    pub dpc_time: Option<f32>,
    pub interrupt_time: Option<f32>,
    /// % Performance Limit: below 1 when the CPU is held back (thermal, power, firmware)
    pub perf_limit: Option<f32>,
    /// hottest ACPI thermal zone, °C; on most machines the zone follows the CPU package
    pub zone_temp: Option<f32>,
    /// per PhysicalDriveN index: (busy 0..1, queue length, ms per transfer, transfers/s)
    pub disks: Vec<(u32, f32, f32, f32, f32)>,
    /// per (process id, adapter LUID): (dedicated, shared) GPU memory in bytes
    pub gpu_proc_mem: HashMap<(u32, u64), (u64, u64)>,
    /// per adapter LUID: (dedicated, shared) GPU memory in use, bytes
    pub gpu_adapter_mem: HashMap<u64, (u64, u64)>,
}

impl Details {
    pub fn new() -> Option<Self> {
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| -> Option<isize> {
                let mut c = 0isize;
                let p = wide(path);
                if PdhAddEnglishCounterW(query, pcwstr(&p), 0, &mut c) == 0 { Some(c) } else { None }
            };
            let d = Self {
                query,
                ctx_switches: add(r"\System\Context Switches/sec"),
                interrupts: add(r"\Processor(_Total)\Interrupts/sec"),
                dpc_time: add(r"\Processor(_Total)\% DPC Time"),
                interrupt_time: add(r"\Processor(_Total)\% Interrupt Time"),
                perf_limit: add(r"\Processor Information(_Total)\% Performance Limit"),
                zone_temp_hp: add(r"\Thermal Zone Information(*)\High Precision Temperature"),
                zone_temp: add(r"\Thermal Zone Information(*)\Temperature"),
                disk_idle: add(r"\PhysicalDisk(*)\% Idle Time"),
                disk_queue: add(r"\PhysicalDisk(*)\Current Disk Queue Length"),
                disk_latency: add(r"\PhysicalDisk(*)\Avg. Disk sec/Transfer"),
                disk_iops: add(r"\PhysicalDisk(*)\Disk Transfers/sec"),
                gpu_proc_dedicated: add(r"\GPU Process Memory(*)\Dedicated Usage"),
                gpu_proc_shared: add(r"\GPU Process Memory(*)\Shared Usage"),
                gpu_adapter_dedicated: add(r"\GPU Adapter Memory(*)\Dedicated Usage"),
                gpu_adapter_shared: add(r"\GPU Adapter Memory(*)\Shared Usage"),
            };
            let _ = PdhCollectQueryData(query);
            Some(d)
        }
    }

    pub fn update(&mut self) -> DetailSample {
        let mut s = DetailSample::default();
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return s;
        }
        let val = |c: Option<isize>| c.and_then(Pdh::value);
        s.ctx_switches = val(self.ctx_switches);
        s.interrupts = val(self.interrupts);
        s.dpc_time = val(self.dpc_time).map(|v| (v / 100.0).clamp(0.0, 1.0) as f32);
        s.interrupt_time = val(self.interrupt_time).map(|v| (v / 100.0).clamp(0.0, 1.0) as f32);
        s.perf_limit = val(self.perf_limit).filter(|v| *v > 0.0).map(|v| (v / 100.0).clamp(0.0, 1.0) as f32);
        s.zone_temp = zone_temp(self.zone_temp_hp, 10.0).or_else(|| zone_temp(self.zone_temp, 1.0));

        // physical disks: instances are "0 C:", "1 D: E:" (and "_Total")
        let index_of = |name: &str| -> Option<u32> { name.split_whitespace().next()?.parse().ok() };
        let arr = |c: Option<isize>| -> Vec<(u32, f64)> { c.map(Pdh::array).unwrap_or_default().into_iter().filter_map(|(n, v)| index_of(&n).map(|i| (i, v))).collect() };
        let (idle, queue, lat, iops) = (arr(self.disk_idle), arr(self.disk_queue), arr(self.disk_latency), arr(self.disk_iops));
        for (i, idle) in &idle {
            let find = |v: &Vec<(u32, f64)>| v.iter().find(|e| e.0 == *i).map(|e| e.1).unwrap_or(0.0);
            let busy = (1.0 - idle / 100.0).clamp(0.0, 1.0) as f32;
            s.disks.push((*i, busy, find(&queue) as f32, (find(&lat) * 1000.0) as f32, find(&iops) as f32));
        }

        let mut sum_pid = |c: Option<isize>, dedicated: bool| {
            for (name, v) in c.map(Pdh::array).unwrap_or_default() {
                let Some(pid) = pid_of(&name) else { continue };
                let e = s.gpu_proc_mem.entry((pid, luid_of(&name))).or_default();
                if dedicated { e.0 += v.max(0.0) as u64 } else { e.1 += v.max(0.0) as u64 }
            }
        };
        sum_pid(self.gpu_proc_dedicated, true);
        sum_pid(self.gpu_proc_shared, false);
        for (c, dedicated) in [(self.gpu_adapter_dedicated, true), (self.gpu_adapter_shared, false)] {
            for (name, v) in c.map(Pdh::array).unwrap_or_default() {
                let e = s.gpu_adapter_mem.entry(luid_of(&name)).or_default();
                if dedicated { e.0 += v.max(0.0) as u64 } else { e.1 += v.max(0.0) as u64 }
            }
        }
        s
    }
}

/// The hottest plausible zone in °C. Zones stuck at the ACPI placeholder 27.8 °C (301 K,
/// firmware without a real sensor) and out-of-range readings are skipped.
fn zone_temp(counter: Option<isize>, per_kelvin: f64) -> Option<f32> {
    counter
        .map(Pdh::array)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, v)| v / per_kelvin - 273.15)
        .filter(|c| *c > 5.0 && *c < 125.0 && (*c - 27.85).abs() > 0.2)
        .fold(None, |m: Option<f64>, c| Some(m.map_or(c, |m| m.max(c))))
        .map(|c| c as f32)
}

impl Drop for Details {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// The adapter LUID in a GPU Engine instance name ("..._luid_0xHIGH_0xLOW_..."); 0 if absent.
fn luid_of(name: &str) -> u64 {
    let Some(rest) = name.split("luid_").nth(1) else { return 0 };
    let mut parts = rest.split('_');
    let hex = |p: Option<&str>| p.and_then(|p| u32::from_str_radix(p.trim_start_matches("0x"), 16).ok());
    match (hex(parts.next()), hex(parts.next())) {
        (Some(hi), Some(lo)) => (hi as u64) << 32 | lo as u64,
        _ => 0,
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}
