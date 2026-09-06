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
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    /// None when GPU counters are unavailable (VMs, old drivers).
    pub gpu_pct: Option<f32>,
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
            let gpu = add(r"\GPU Engine(*)\Utilization Percentage");
            if gpu.is_none() {
                crate::log!("GPU Engine counters unavailable; gpu widget will show --");
            }
            let _ = PdhCollectQueryData(query);
            Some(Self { query, disk_read, disk_write, gpu, disk_read_all, disk_write_all, disk_read_bps: 0.0, disk_write_bps: 0.0, gpu_pct: None })
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

    fn value(counter: isize) -> Option<f64> {
        let mut v = PDH_FMT_COUNTERVALUE::default();
        let r = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut v) };
        if r == 0 { Some(unsafe { v.Anonymous.doubleValue }) } else { None }
    }

    /// Task Manager style: sum utilisation per engine type, report the busiest engine type.
    fn gpu_value(counter: isize) -> Option<f32> {
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
            let mut per_type: HashMap<String, f64> = HashMap::new();
            for it in slice {
                if it.FmtValue.CStatus != 0 {
                    continue;
                }
                let name = it.szName.to_string().unwrap_or_default();
                let engtype = name.rsplit("engtype_").next().unwrap_or("").to_string();
                *per_type.entry(engtype).or_default() += it.FmtValue.Anonymous.doubleValue;
            }
            per_type.values().cloned().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
                .map(|v| (v / 100.0).clamp(0.0, 1.0) as f32)
        }
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
            self.gpu_pct = Self::gpu_value(g).or(self.gpu_pct);
        }
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}
