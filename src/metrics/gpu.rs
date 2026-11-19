//! GPU identity and VRAM usage through DXGI: every hardware adapter with its vendor/device id,
//! dedicated memory and user-mode driver version; VRAM usage for the primary one (DXGI 1.4).
//! Adapters can be switched off (popup click, config `gpu_disabled`): the load then covers the
//! others only and the primary is picked among them.

use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[derive(Clone, Debug)]
pub struct AdapterInfo {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated: u64,
    /// user-mode driver version as reported by DXGI, e.g. "32.0.101.6790"
    pub driver: String,
    /// adapter LUID, as in the "luid_0x..._0x..." part of the GPU Engine counter instances
    pub luid: u64,
    /// counted in the load and eligible as the primary adapter
    pub enabled: bool,
    /// busiest engine type of this adapter in 0..1; None until the counters have a sample
    pub load: Option<f32>,
    /// temperature, clocks, power; read for the primary adapter while the popup is open
    pub perf: super::gpuperf::Perf,
    /// GPU memory in use on this adapter (popup only, GPU Adapter Memory counters), bytes
    pub dedicated_used: Option<u64>,
    pub shared_used: Option<u64>,
    /// device power state from Windows (0 = D0 awake, 3 = D3 asleep); battery popup only
    pub d_state: Option<u8>,
}

/// The chip's own graphics among the adapters: the processor maker's adapter (Intel 0x8086,
/// AMD 0x1002) with the least dedicated memory. Every other real adapter is a discrete card.
pub fn integrated_index(adapters: &[AdapterInfo], cpu_name: &str) -> Option<usize> {
    let l = cpu_name.to_lowercase();
    let vendor = if l.contains("intel") || l.contains("core") {
        0x8086
    } else if l.contains("amd") || l.contains("ryzen") {
        0x1002
    } else {
        return None;
    };
    adapters.iter().enumerate().filter(|(_, a)| a.vendor_id == vendor).min_by_key(|(_, a)| a.dedicated).map(|(i, _)| i)
}

/// The discrete cards: every real adapter that is not the chip's own graphics. Windows can list
/// one chip more than once (same vendor and device ID, another LUID): that is still one chip.
pub fn discrete_indices(adapters: &[AdapterInfo], cpu_name: &str) -> Vec<usize> {
    let igpu = integrated_index(adapters, cpu_name);
    let own = igpu.map(|i| (adapters[i].vendor_id, adapters[i].device_id));
    let mut seen: Vec<(u32, u32)> = Vec::new();
    let mut out = Vec::new();
    for (i, a) in adapters.iter().enumerate() {
        let id = (a.vendor_id, a.device_id);
        if Some(id) == own || is_virtual(a) || seen.contains(&id) {
            continue;
        }
        seen.push(id);
        out.push(i);
    }
    out
}

/// For each adapter, the earlier entry it repeats. Hybrid notebooks can list one chip twice
/// (same vendor and device ID, another LUID); two cards of one model sit on different PCI
/// buses and stay apart. An unknown bus counts as the same chip.
pub fn twins(adapters: &[AdapterInfo], bus: &dyn Fn(u64) -> Option<u32>) -> Vec<Option<usize>> {
    let mut out: Vec<Option<usize>> = vec![None; adapters.len()];
    for i in 0..adapters.len() {
        let b = &adapters[i];
        out[i] = (0..i).filter(|&k| out[k].is_none()).find(|&k| {
            let a = &adapters[k];
            a.vendor_id == b.vendor_id
                && a.device_id == b.device_id
                && match (bus(a.luid), bus(b.luid)) {
                    (Some(x), Some(y)) => x == y,
                    _ => true,
                }
        });
    }
    out
}

/// Software and virtual adapters (Microsoft Basic Render, remote / virtual displays).
pub fn is_virtual(a: &AdapterInfo) -> bool {
    a.vendor_id == 0x1414 || a.name.to_lowercase().contains("virtual") || a.name.contains("Basic Render")
}

/// Power state (0..3 for D0..D3) of each display device as (vendor, device) from Windows' own
/// device data: asks the driver stack nothing, so a sleeping GPU stays asleep.
pub fn power_states() -> Vec<(u32, u32, u8)> {
    use windows::core::GUID;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDevicePropertyW, DIGCF_PRESENT, SP_DEVINFO_DATA};
    use windows::Win32::Devices::Properties::{DEVPROPKEY, DEVPROPTYPE};
    const DISPLAY: GUID = GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);
    // DEVPKEY_Device_HardwareIds and DEVPKEY_Device_PowerData (CM_POWER_DATA)
    const HWIDS: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 3 };
    const POWER: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 32 };
    let mut out = Vec::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(Some(&DISPLAY), None, None, DIGCF_PRESENT) else { return out };
        let mut i = 0;
        loop {
            let mut info = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInfo(set, i, &mut info).is_err() {
                break;
            }
            i += 1;
            let mut ty = DEVPROPTYPE(0);
            let mut ids = [0u8; 1024];
            let mut got = 0u32;
            if SetupDiGetDevicePropertyW(set, &info, &HWIDS, &mut ty, Some(&mut ids), Some(&mut got), 0).is_err() {
                continue;
            }
            let w: Vec<u16> = ids[..got as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let first = String::from_utf16_lossy(&w).split('\0').next().unwrap_or_default().to_uppercase();
            let hex = |key: &str| first.find(key).and_then(|p| u32::from_str_radix(first.get(p + key.len()..p + key.len() + 4)?, 16).ok());
            let (Some(ven), Some(dev)) = (hex("VEN_"), hex("DEV_")) else { continue };
            // CM_POWER_DATA: PD_Size, then PD_MostRecentPowerState (1 = D0 .. 4 = D3)
            let mut pd = [0u8; 64];
            if SetupDiGetDevicePropertyW(set, &info, &POWER, &mut ty, Some(&mut pd), None, 0).is_err() {
                continue;
            }
            let state = i32::from_le_bytes([pd[4], pd[5], pd[6], pd[7]]);
            if (1..=4).contains(&state) {
                out.push((ven, dev, (state - 1) as u8));
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// PCI vendor of the processor's own graphics, from CPUID: Intel 0x8086, AMD 0x1002. None on
/// other processors (ARM, or an unknown maker).
pub fn cpu_graphics_vendor() -> Option<u32> {
    #[cfg(target_arch = "x86_64")]
    {
        // CPUID leaf 0: the vendor string in EBX, EDX, ECX
        #[allow(unused_unsafe)]
        let r = unsafe { std::arch::x86_64::__cpuid(0) };
        let mut v = Vec::with_capacity(12);
        for reg in [r.ebx, r.edx, r.ecx] {
            v.extend_from_slice(&reg.to_le_bytes());
        }
        match &v[..] {
            b"GenuineIntel" => Some(0x8086),
            b"AuthenticAMD" => Some(0x1002),
            _ => None,
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// The display devices of another maker than the processor's graphics (the discrete cards),
/// once per (vendor, device), with whether each is awake (D0). Same-maker cards (an Arc card
/// next to Intel graphics) are not told apart here. Empty when the processor is unknown.
pub fn discrete_states(states: &[(u32, u32, u8)], igpu_vendor: Option<u32>) -> Vec<(u32, u32, bool)> {
    let Some(own) = igpu_vendor else { return Vec::new() };
    let mut out: Vec<(u32, u32, bool)> = Vec::new();
    for &(ven, dev, d) in states {
        if ven == own || ven == 0x1414 {
            continue;
        }
        match out.iter_mut().find(|(v, dv, _)| *v == ven && *dv == dev) {
            // one chip listed twice: awake if either entry is
            Some(e) => e.2 |= d == 0,
            None => out.push((ven, dev, d == 0)),
        }
    }
    out
}

impl AdapterInfo {
    pub fn vendor(&self) -> &'static str {
        match self.vendor_id {
            0x10DE => "NVIDIA",
            0x1002 | 0x1022 => "AMD",
            0x8086 => "Intel",
            0x1414 => "Microsoft",
            0x5143 => "Qualcomm",
            0x15AD => "VMware",
            0x1AB8 => "Parallels",
            0x80EE => "VirtualBox",
            _ => "",
        }
    }
}

pub struct Gpu {
    /// DXGI handle of each entry in `adapters`, for the VRAM usage of the primary one
    handles: Vec<Option<IDXGIAdapter3>>,
    /// (LUID of a listed adapter, LUID of its repeat): the counters of both count for it
    alias: Vec<(u64, u64)>,
    primary: Option<usize>,
    /// sensor thread, started on the first GPU popup
    worker: Option<SensorWorker>,
    pub name: String,
    pub vram_total: u64,
    pub vram_used: u64,
    pub vram_budget: u64,
    pub adapters: Vec<AdapterInfo>,
}

impl Gpu {
   pub fn new(disabled: &[String]) -> Self {
        let mut g = Self { handles: Vec::new(), alias: Vec::new(), primary: None, worker: None, name: String::new(), vram_total: 0, vram_used: 0, vram_budget: 0, adapters: Vec::new() };
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return g };
            let mut i = 0;
            while let Ok(a) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = a.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let raw = String::from_utf16_lossy(&desc.Description);
                // Windows 7 AMD drivers append "   \Registry\Machine\..." after the name
                let name = raw.split(['\0', '\\']).next().unwrap_or("").trim().to_string();
                let driver = a
                    .CheckInterfaceSupport(&IDXGIDevice::IID)
                    .map(|v| {
                        let v = v as u64;
                        format!("{}.{}.{}.{}", (v >> 48) & 0xFFFF, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
                    })
                    .unwrap_or_default();
                let luid = (desc.AdapterLuid.HighPart as u32 as u64) << 32 | desc.AdapterLuid.LowPart as u64;
                g.adapters.push(AdapterInfo {
                    name,
                    vendor_id: desc.VendorId,
                    device_id: desc.DeviceId,
                    dedicated: desc.DedicatedVideoMemory as u64,
                    driver,
                    luid,
                    enabled: true,
                    load: None,
                    perf: Default::default(),
                    dedicated_used: None,
                    shared_used: None,
                    d_state: None,
                });
                g.handles.push(a.cast::<IDXGIAdapter3>().ok());
            }
        }
        g.merge_twins();
        g.set_disabled(disabled);
        g
    }

    /// One row per chip: a repeated entry is dropped and its LUID kept as an alias.
    fn merge_twins(&mut self) {
        let a = &self.adapters;
        let same = |i: usize, k: usize| a[i].vendor_id == a[k].vendor_id && a[i].device_id == a[k].device_id;
        let cand: Vec<u64> = (0..a.len()).filter(|&i| (0..a.len()).any(|k| k != i && same(i, k))).map(|i| a[i].luid).collect();
        if cand.is_empty() {
            return;
        }
        // kernel adapter handles for the bus numbers only; closed again on drop
        let kmt = super::gpuperf::GpuPerf::new(&cand);
        let tw = twins(a, &|l| kmt.as_ref().and_then(|p| p.bus(l)));
        drop(kmt);
        for i in (0..tw.len()).rev() {
            if let Some(k) = tw[i] {
                self.alias.push((self.adapters[k].luid, self.adapters[i].luid));
                self.adapters.remove(i);
                self.handles.remove(i);
            }
        }
    }

    /// Every LUID, repeats included (for the counter sources).
    pub fn all_luids(&self) -> Vec<u64> {
        self.adapters.iter().map(|a| a.luid).chain(self.alias.iter().map(|x| x.1)).collect()
    }

    /// The LUIDs whose counters belong to the adapter with this LUID.
    fn luids_of(&self, luid: u64) -> impl Iterator<Item = u64> + '_ {
        std::iter::once(luid).chain(self.alias.iter().filter(move |x| x.0 == luid).map(|x| x.1))
    }

    /// Switch adapters off by name (at least one stays on) and pick the primary among the rest.
    pub fn set_disabled(&mut self, disabled: &[String]) {
        for a in self.adapters.iter_mut() {
            a.enabled = !disabled.contains(&a.name);
        }
        if !self.adapters.iter().any(|a| a.enabled) {
            self.adapters.iter_mut().for_each(|a| a.enabled = true);
        }
        // the enabled adapter with the most dedicated memory: the discrete card over the iGPU
        self.primary = self
            .adapters
            .iter()
            .enumerate()
            .filter(|(_, a)| a.enabled)
            .max_by_key(|(i, a)| (a.dedicated, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        let p = self.primary.map(|i| &self.adapters[i]);
        self.name = p.map(|a| a.name.clone()).unwrap_or_default();
        self.vram_total = p.map(|a| a.dedicated).unwrap_or(0);
        self.vram_used = 0;
        self.vram_budget = 0;
        self.update();
    }

    /// The adapter the widgets describe.
    pub fn primary(&self) -> Option<&AdapterInfo> {
        self.primary.map(|i| &self.adapters[i])
    }

    /// Take the per-adapter loads of the GPU Engine counters (by LUID); returns the load shown:
    /// the busiest enabled adapter, or the busiest of all when no LUID matched an adapter.
    pub fn set_loads(&mut self, loads: &std::collections::HashMap<u64, f32>) -> Option<f32> {
        if loads.is_empty() {
            return None;
        }
        let mut best: Option<f32> = None;
        for i in 0..self.adapters.len() {
            let v = self.luids_of(self.adapters[i].luid).filter_map(|l| loads.get(&l).cloned()).reduce(f32::max);
            let a = &mut self.adapters[i];
            a.load = Some(v.unwrap_or(0.0));
            if let (true, Some(v)) = (a.enabled, v) {
                best = Some(best.map_or(v, |b| b.max(v)));
            }
        }
        best.or_else(|| loads.values().cloned().reduce(f32::max))
    }

    /// Sensors of the primary adapter (popup only: two kernel queries per call).
    /// Memory in use per adapter from the GPU Adapter Memory counters (by LUID).
    /// An adapter missing from one sample keeps its last figures, so the row does not blink.
    pub fn set_adapter_mem(&mut self, mem: &std::collections::HashMap<u64, (u64, u64)>) {
        for i in 0..self.adapters.len() {
            // a repeat's memory is the same memory: take the larger figure, not the sum
            let m = self.luids_of(self.adapters[i].luid).filter_map(|l| mem.get(&l).cloned()).reduce(|x, y| (x.0.max(y.0), x.1.max(y.1)));
            if let Some(m) = m {
                let a = &mut self.adapters[i];
                a.dedicated_used = Some(m.0);
                a.shared_used = Some(m.1);
            }
        }
    }

    /// Sensors of the primary adapter, read on the sensor thread (the vendor libraries and a
    /// sleeping Optimus GPU take hundreds of ms per read): asks for a fresh reading and takes the
    /// last one. GPU popup only; `stop_perf` lets the thread sleep again.
    pub fn update_perf(&mut self) {
        let Some(i) = self.primary else { return };
        self.update_perf_for(i);
    }

    /// Power states read elsewhere (the controls thread).
    pub fn set_power_states(&mut self, states: &[(u32, u32, u8)]) {
        for a in &mut self.adapters {
            a.d_state = states.iter().find(|(v, d, _)| *v == a.vendor_id && *d == a.device_id).map(|s| s.2);
        }
    }

    /// Sensors of adapter `i`, as `update_perf`.
    pub fn update_perf_for(&mut self, i: usize) {
        let Some(a) = self.adapters.get(i) else { return };
        let target = Target { luid: a.luid, vendor_id: a.vendor_id, name: a.name.clone() };
        let luids = self.all_luids();
        let worker = self.worker.get_or_insert_with(|| SensorWorker::start(luids));
        let (m, cv) = &*worker.st;
        let mut st = m.lock().unwrap();
        if st.want.as_ref() != Some(&target) {
            st.want = Some(target.clone());
            cv.notify_all();
        }
        let fresh = st.result.filter(|r| r.0 == target.luid).map(|r| r.1);
        drop(st);
        // until a reading arrives, and for the values it lacks, the last known ones stay
        if let Some(perf) = fresh {
            self.adapters[i].perf = perf.or(self.adapters[i].perf);
        }
    }

    pub fn stop_perf(&mut self) {
        if let Some(w) = &self.worker {
            w.st.0.lock().unwrap().want = None;
        }
    }

    pub fn update(&mut self) {
        let Some(a) = self.primary.and_then(|i| self.handles[i].as_ref()) else { return };
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        if unsafe { a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }.is_ok() {
            self.vram_used = info.CurrentUsage;
            self.vram_budget = info.Budget;
        }
    }
}

/// The adapter the sensor thread reads.
#[derive(Clone, PartialEq)]
struct Target {
    luid: u64,
    vendor_id: u32,
    name: String,
}

#[derive(Default)]
struct SensorState {
    /// what to read; None: sleep
    want: Option<Target>,
    /// LUID and the last reading
    result: Option<(u64, super::gpuperf::Perf)>,
}

struct SensorWorker {
    st: std::sync::Arc<(std::sync::Mutex<SensorState>, std::sync::Condvar)>,
}

impl SensorWorker {
    fn start(luids: Vec<u64>) -> Self {
        let st = std::sync::Arc::new((std::sync::Mutex::new(SensorState::default()), std::sync::Condvar::new()));
        let shared = st.clone();
        let _ = std::thread::Builder::new().name("gpu-sensors".into()).spawn(move || sensor_loop(shared, luids));
        Self { st }
    }
}

/// D3DKMT for every vendor, then the vendor library on top; one reading a second while wanted.
/// The libraries are loaded here on first use and stay on this thread.
fn sensor_loop(st: std::sync::Arc<(std::sync::Mutex<SensorState>, std::sync::Condvar)>, luids: Vec<u64>) {
    // WINSTATS_WIN7_GPU: no D3DKMT perf data, as on Windows 7 (testing)
    let mut sensors = if std::env::var_os("WINSTATS_WIN7_GPU").is_some() { None } else { super::gpuperf::GpuPerf::new(&luids) };
    let mut nvidia: Option<Option<super::nvidia::Nvidia>> = None;
    let mut intel: Option<Option<super::intel::Intel>> = None;
    let mut amd: Option<Option<super::amd::Amd>> = None;
    let (m, cv) = &*st;
    loop {
        let target = {
            let mut s = m.lock().unwrap();
            while s.want.is_none() {
                s = cv.wait(s).unwrap();
            }
            s.want.clone().unwrap()
        };
        let luid = target.luid;
        let mut perf = sensors.as_mut().map(|t| t.read(luid)).unwrap_or_default();
        let bus = sensors.as_ref().and_then(|t| t.bus(luid));
        if target.vendor_id == 0x10DE {
            if let Some(nv) = nvidia.get_or_insert_with(|| Some(super::nvidia::Nvidia::new())) {
                nv.fill(bus, &target.name, &mut perf);
            }
        }
        if target.vendor_id == 0x1002 {
            if let Some(a) = amd.get_or_insert_with(super::amd::Amd::new).as_ref() {
                a.fill(bus, &mut perf);
            }
        }
        if target.vendor_id == 0x8086 {
            if let Some(i) = intel.get_or_insert_with(super::intel::Intel::new).as_mut() {
                i.fill(luid, &mut perf);
            }
        }
        let s = m.lock().unwrap();
        let mut s = s;
        s.result = Some((luid, perf));
        // next reading in a second, or at once when the adapter changes
        let _ = cv.wait_timeout_while(s, std::time::Duration::from_secs(1), |s| s.want.as_ref() == Some(&target)).unwrap();
    }
}

#[cfg(test)]
mod power_state_tests {
    fn ad(name: &str, vendor_id: u32, device_id: u32, dedicated: u64, luid: u64) -> super::AdapterInfo {
        super::AdapterInfo { name: name.into(), vendor_id, device_id, dedicated, driver: String::new(), luid, enabled: true, load: None, perf: Default::default(), dedicated_used: None, shared_used: None, d_state: None }
    }

    #[test]
    fn one_chip_listed_twice_is_no_graphics_card() {
        let cpu = "Intel(R) Core(TM) Ultra 9 285H";
        // the Arc 140T twice (two LUIDs), no card: nothing discrete
        let a = [ad("Intel(R) Arc(TM) 140T GPU", 0x8086, 0x7d51, 128 << 20, 1), ad("Intel(R) Arc(TM) 140T GPU", 0x8086, 0x7d51, 128 << 20, 2)];
        assert!(super::discrete_indices(&a, cpu).is_empty());
        // with a GeForce (listed twice too) and the basic renderer: the GeForce once
        let b = [ad("Intel(R) UHD Graphics", 0x8086, 0xa788, 128 << 20, 1), ad("NVIDIA GeForce RTX 4090 Laptop GPU", 0x10de, 0x2757, 16 << 30, 2), ad("NVIDIA GeForce RTX 4090 Laptop GPU", 0x10de, 0x2757, 16 << 30, 3), ad("Microsoft Basic Render Driver", 0x1414, 0x8c, 0, 4)];
        assert_eq!(super::discrete_indices(&b, "13th Gen Intel(R) Core(TM) i9-13900HX"), vec![1]);
        // an Intel Arc card next to Intel graphics: another device ID, so a card
        let c = [ad("Intel(R) Graphics", 0x8086, 0x7d55, 128 << 20, 1), ad("Intel(R) Arc(TM) A770M", 0x8086, 0x5690, 16 << 30, 2)];
        assert_eq!(super::discrete_indices(&c, cpu), vec![1]);
    }

    #[test]
    fn a_chip_listed_twice_is_one_row() {
        // a hybrid notebook: Intel UHD, the GeForce, Intel UHD again under another LUID
        let a = [ad("Intel(R) UHD Graphics", 0x8086, 0xa788, 128 << 20, 1), ad("NVIDIA GeForce RTX 4090 Laptop GPU", 0x10de, 0x2757, 16 << 30, 2), ad("Intel(R) UHD Graphics", 0x8086, 0xa788, 128 << 20, 3)];
        // both Intel entries on bus 0, the GeForce on bus 1
        let bus = |l: u64| Some(if l == 2 { 1 } else { 0 });
        assert_eq!(super::twins(&a, &bus), vec![None, None, Some(0)]);
        // bus unknown (no kernel handle): still the same chip
        assert_eq!(super::twins(&a, &|_| None), vec![None, None, Some(0)]);
        // two cards of one model on their own buses: both stay
        let b = [ad("NVIDIA GeForce RTX 4090", 0x10de, 0x2684, 24 << 30, 1), ad("NVIDIA GeForce RTX 4090", 0x10de, 0x2684, 24 << 30, 2)];
        assert_eq!(super::twins(&b, &|l| Some(l as u32)), vec![None, None]);
    }

    #[test]
    fn discrete_states_by_maker() {
        // Intel UHD awake, the GeForce asleep (listed twice), the basic renderer ignored
        let s = [(0x8086, 0xa788, 0), (0x10de, 0x2757, 3), (0x10de, 0x2757, 3), (0x1414, 0x8c, 0)];
        assert_eq!(super::discrete_states(&s, Some(0x8086)), vec![(0x10de, 0x2757, false)]);
        // one of the two entries awake: awake
        let s = [(0x8086, 0xa788, 0), (0x10de, 0x2757, 3), (0x10de, 0x2757, 0)];
        assert_eq!(super::discrete_states(&s, Some(0x8086)), vec![(0x10de, 0x2757, true)]);
        // only the chip's own graphics, or an unknown processor: nothing
        assert!(super::discrete_states(&[(0x8086, 0x7d51, 0)], Some(0x8086)).is_empty());
        assert!(super::discrete_states(&s, None).is_empty());
    }

    #[test]
    fn this_processor_is_known() {
        let v = super::cpu_graphics_vendor();
        assert!(v == Some(0x8086) || v == Some(0x1002), "{v:x?}");
    }

    #[test]
    #[ignore]
    fn print_power_states() {
        let s = super::power_states();
        println!("{:x?}", s);
        println!("igpu vendor {:x?}, discrete {:x?}", super::cpu_graphics_vendor(), super::discrete_states(&s, super::cpu_graphics_vendor()));
    }
}
