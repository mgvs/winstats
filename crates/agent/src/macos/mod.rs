//! macOS sources `sysinfo` does not cover: the GPU (IOAccelerator), the battery (IOPowerSources),
//! fans and system power (SMC), Apple silicon clocks and power (IOReport) and what kind each
//! network interface is (SystemConfiguration). Thin CoreFoundation / IOKit bindings live here.

pub mod battery;
pub mod disk;
pub mod gpu;
pub mod ioreport;
pub mod net;
pub mod smc;
pub mod sys;

use std::ffi::{c_char, c_void, CString};
use winstats_proto::{BatterySample, FanSample, GpuSample, PowerSample};

pub type CFTypeRef = *const c_void;
pub type CFIndex = isize;
pub type IoObject = u32;

const UTF8: u32 = 0x0800_0100;
const NUMBER_I64: CFIndex = 4;
const NUMBER_F64: CFIndex = 13;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, enc: u32) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, len: CFIndex, enc: u32) -> bool;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> bool;
    fn CFBooleanGetValue(b: CFTypeRef) -> bool;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    pub fn CFArrayGetCount(a: CFTypeRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(a: CFTypeRef, i: CFIndex) -> CFTypeRef;
    fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFTypeRef;
    fn IOServiceGetMatchingServices(main_port: u32, matching: CFTypeRef, it: *mut IoObject) -> i32;
    fn IOIteratorNext(it: IoObject) -> IoObject;
    pub fn IOObjectRelease(o: IoObject) -> i32;
    fn IORegistryEntryCreateCFProperty(entry: IoObject, key: CFTypeRef, alloc: CFTypeRef, opts: u32) -> CFTypeRef;
    fn IORegistryEntryGetParentEntry(entry: IoObject, plane: *const c_char, parent: *mut IoObject) -> i32;
}

/// An owned CoreFoundation object, released on drop.
pub struct Cf(pub CFTypeRef);

// SAFETY: CoreFoundation retain / release is thread-safe and the objects kept are not shared.
unsafe impl Send for Cf {}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

impl Cf {
    /// Takes ownership of a "Create" / "Copy" result; None for null.
    pub fn new(p: CFTypeRef) -> Option<Cf> {
        (!p.is_null()).then_some(Cf(p))
    }
}

pub fn cfstr(s: &str) -> Cf {
    let c = CString::new(s).unwrap_or_default();
    Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) })
}

fn is(p: CFTypeRef, id: unsafe extern "C" fn() -> usize) -> bool {
    !p.is_null() && unsafe { CFGetTypeID(p) == id() }
}

/// Borrowed value of a dictionary, null when missing.
pub fn get(dict: CFTypeRef, key: &str) -> CFTypeRef {
    if dict.is_null() {
        return std::ptr::null();
    }
    let k = cfstr(key);
    unsafe { CFDictionaryGetValue(dict, k.0) }
}

pub fn string(p: CFTypeRef) -> Option<String> {
    if !is(p, CFStringGetTypeID) {
        return None;
    }
    let mut buf = vec![0 as c_char; 512];
    unsafe { CFStringGetCString(p, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) }.then(|| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().to_string())
}

pub fn int(p: CFTypeRef) -> Option<i64> {
    if is(p, CFBooleanGetTypeID) {
        return Some(unsafe { CFBooleanGetValue(p) } as i64);
    }
    if !is(p, CFNumberGetTypeID) {
        return None;
    }
    let mut v = 0i64;
    unsafe { CFNumberGetValue(p, NUMBER_I64, &mut v as *mut i64 as *mut c_void) }.then_some(v)
}

pub fn float(p: CFTypeRef) -> Option<f64> {
    if !is(p, CFNumberGetTypeID) {
        return int(p).map(|v| v as f64);
    }
    let mut v = 0f64;
    unsafe { CFNumberGetValue(p, NUMBER_F64, &mut v as *mut f64 as *mut c_void) }.then_some(v)
}

pub fn boolean(p: CFTypeRef) -> Option<bool> {
    int(p).map(|v| v != 0)
}

/// Bytes of a CFData; a registry string property ("model" of a PCI device) is often one.
pub fn data(p: CFTypeRef) -> Option<Vec<u8>> {
    if !is(p, CFDataGetTypeID) {
        return None;
    }
    let n = unsafe { CFDataGetLength(p) } as usize;
    let ptr = unsafe { CFDataGetBytePtr(p) };
    (!ptr.is_null()).then(|| unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec())
}

/// An IOKit object, released on drop.
pub struct Io(pub IoObject);

impl Drop for Io {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

impl Io {
    /// Every registry entry of an IOKit class ("IOAccelerator", "AppleARMIODevice").
    pub fn services(class: &str) -> Vec<Io> {
        let c = CString::new(class).unwrap_or_default();
        let mut it: IoObject = 0;
        let mut out = Vec::new();
        unsafe {
            let m = IOServiceMatching(c.as_ptr());
            if m.is_null() || IOServiceGetMatchingServices(0, m, &mut it) != 0 {
                return out;
            }
            loop {
                let o = IOIteratorNext(it);
                if o == 0 {
                    break;
                }
                out.push(Io(o));
            }
            IOObjectRelease(it);
        }
        out
    }

    pub fn prop(&self, key: &str) -> Option<Cf> {
        let k = cfstr(key);
        Cf::new(unsafe { IORegistryEntryCreateCFProperty(self.0, k.0, std::ptr::null(), 0) })
    }

    pub fn parent(&self) -> Option<Io> {
        let mut p: IoObject = 0;
        let plane = CString::new("IOService").unwrap_or_default();
        (unsafe { IORegistryEntryGetParentEntry(self.0, plane.as_ptr(), &mut p) } == 0 && p != 0).then_some(Io(p))
    }

    /// A string property that may be stored as CFString or as NUL-terminated CFData.
    pub fn text(&self, key: &str) -> Option<String> {
        let v = self.prop(key)?;
        string(v.0).or_else(|| data(v.0).map(|b| String::from_utf8_lossy(b.split(|c| *c == 0).next().unwrap_or(&[])).trim().to_string())).filter(|s| !s.is_empty())
    }
}

/// A numeric sysctl ("hw.perflevel0.cpusperl2"); None when the Mac has no such key.
pub fn sysctl(name: &str) -> Option<u64> {
    let c = CString::new(name).ok()?;
    let mut v = 0u64;
    let mut len = std::mem::size_of::<u64>();
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), &mut v as *mut u64 as *mut c_void, &mut len, std::ptr::null_mut(), 0) };
    // 4-byte values land in the low half (little-endian)
    (r == 0).then_some(if len == 4 { v & 0xffff_ffff } else { v })
}

/// Everything macOS adds to a snapshot.
#[derive(Default)]
pub struct MacSample {
    /// MHz per logical core; empty when unknown
    pub freq: Vec<u32>,
    pub gpu: Option<GpuSample>,
    pub battery: Option<BatterySample>,
    pub fans: Vec<FanSample>,
    pub power: Vec<PowerSample>,
    /// transfers per second and ms per transfer of the internal disk
    pub disk: Option<disk::Activity>,
}

pub struct Mac {
    gpu: Option<gpu::MacGpu>,
    smc: Option<smc::Smc>,
    ior: Option<ioreport::IoReport>,
    /// IOReport cluster of each logical core
    core_cluster: Vec<Option<usize>>,
    disk: Option<disk::Disk>,
}

impl Mac {
    /// `classes`: per logical core, 0 = efficiency, 1 = performance (as `topology` reports them).
    pub fn new(classes: &[u8]) -> Mac {
        let ior = ioreport::IoReport::open();
        let mut core_cluster = vec![None; classes.len()];
        if let Some(r) = &ior {
            // cores of a kind are numbered cluster by cluster, `cpusperl2` to a cluster
            for (class, efficiency, level) in [(0u8, true, "hw.perflevel1"), (1, false, "hw.perflevel0")] {
                let ids: Vec<usize> = r.clusters.iter().enumerate().filter(|(_, c)| c.efficiency == efficiency).map(|(i, _)| i).collect();
                let per = sysctl(&format!("{level}.cpusperl2")).unwrap_or(0) as usize;
                let cores: Vec<usize> = (0..classes.len()).filter(|i| classes[*i] == class).collect();
                for (k, core) in cores.iter().enumerate() {
                    let at = if per > 0 { k / per } else { 0 };
                    core_cluster[*core] = ids.get(at.min(ids.len().saturating_sub(1))).copied();
                }
            }
        }
        Mac { gpu: gpu::MacGpu::find(), smc: smc::Smc::open(), ior, core_cluster, disk: disk::Disk::find() }
    }

    pub fn gpu_name(&self) -> Option<String> {
        self.gpu.as_ref().map(|g| g.name.clone())
    }

    pub fn gpus(&self) -> Vec<winstats_proto::GpuInfo> {
        self.gpu.as_ref().map(|g| g.all.clone()).unwrap_or_default()
    }

    pub fn disk_hw(&self) -> Option<disk::Hw> {
        self.disk.as_ref().map(|d| d.hw.clone())
    }

    /// The highest clock of any cluster (Apple silicon), MHz.
    pub fn cpu_max_mhz(&self) -> Option<u32> {
        self.ior.as_ref().and_then(|r| r.clusters.iter().filter_map(|c| c.max_mhz()).max())
    }

    pub fn has_battery(&self) -> bool {
        battery::read().is_some()
    }

    pub fn sample(&mut self) -> MacSample {
        let mut out = MacSample { battery: battery::read(), disk: self.disk.as_mut().and_then(|d| d.activity()), ..Default::default() };
        // the pack's temperature from SMC where the battery does not say it
        if let (Some(b), Some(smc)) = (out.battery.as_mut(), self.smc.as_ref()) {
            if b.temp_c.is_none() {
                b.temp_c = smc.battery_temp();
            }
        }
        let reading = self.ior.as_mut().map(|r| r.sample());
        if let (Some(r), Some(ior)) = (&reading, &self.ior) {
            if self.core_cluster.iter().all(|c| c.is_some()) {
                // a cluster that slept through the interval sits at its lowest clock
                out.freq = self.core_cluster.iter().map(|c| c.and_then(|i| r.cluster_mhz.get(i).copied().flatten().or_else(|| ior.clusters[i].min_mhz())).unwrap_or(0)).collect();
            }
        }
        if let Some(g) = &self.gpu {
            let mut s = g.sample();
            if let (Some(r), Some(ior)) = (&reading, &self.ior) {
                s.core_mhz = r.gpu_mhz.or(ior.gpu_min_mhz());
                s.core_max_mhz = ior.gpu_max_mhz;
                s.power_w = r.gpu_w;
            }
            s.temp = self.smc.as_ref().and_then(|m| m.gpu_temp());
            out.gpu = Some(s);
        }
        if let Some(smc) = &self.smc {
            out.fans = smc.fans();
            if let Some(w) = smc.system_power() {
                out.power.push(PowerSample { id: "System".into(), w });
            }
        }
        if let Some(w) = reading.as_ref().and_then(|r| r.cpu_w) {
            out.power.push(PowerSample { id: "CPU".into(), w });
        }
        out
    }
}
