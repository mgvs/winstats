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
        let mut g = Self { handles: Vec::new(), primary: None, worker: None, name: String::new(), vram_total: 0, vram_used: 0, vram_budget: 0, adapters: Vec::new() };
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
                });
                g.handles.push(a.cast::<IDXGIAdapter3>().ok());
            }
        }
        g.set_disabled(disabled);
        g
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
        for a in self.adapters.iter_mut() {
            let v = loads.get(&a.luid).cloned();
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
        for a in self.adapters.iter_mut() {
            if let Some(m) = mem.get(&a.luid) {
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
        let a = &self.adapters[i];
        let target = Target { luid: a.luid, vendor_id: a.vendor_id, name: a.name.clone() };
        let luids: Vec<u64> = self.adapters.iter().map(|a| a.luid).collect();
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
