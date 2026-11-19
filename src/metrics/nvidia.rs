//! NVIDIA extras from the libraries the driver installs: NVML (nvml.dll) for board power and its
//! limit, PCIe link and fan duty, and temperature and clocks where D3DKMT has none (Windows 7),
//! NVAPI (nvapi64.dll) for the hot spot. Both are loaded at run
//! time on the first GPU popup; a missing library, a missing entry point or a failing call only
//! leaves the values empty. NVML can wake a sleeping Optimus GPU, so it is asked only while the
//! GPU popup is open.

use super::gpuperf::Perf;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, PCWSTR};

type Device = *mut core::ffi::c_void;

/// nvmlPciInfo_t
#[repr(C)]
struct PciInfo {
    bus_id_legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    bus_id: [u8; 32],
}

type Fn0 = unsafe extern "C" fn() -> i32;
type FnCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnHandle = unsafe extern "C" fn(u32, *mut Device) -> i32;
type FnPci = unsafe extern "C" fn(Device, *mut PciInfo) -> i32;
type FnU32 = unsafe extern "C" fn(Device, *mut u32) -> i32;
/// nvmlDeviceGetTemperature(device, sensor, value) / nvmlDeviceGetClockInfo(device, type, value)
type FnKindU32 = unsafe extern "C" fn(Device, u32, *mut u32) -> i32;

/// nvmlTemperatureSensors_t / nvmlClockType_t
const NVML_TEMPERATURE_GPU: u32 = 0;
const NVML_CLOCK_GRAPHICS: u32 = 0;
const NVML_CLOCK_MEM: u32 = 2;

struct Nvml {
    power: Option<FnU32>,
    limit: Option<FnU32>,
    gen: Option<FnU32>,
    width: Option<FnU32>,
    max_gen: Option<FnU32>,
    max_width: Option<FnU32>,
    fan: Option<FnU32>,
    temp: Option<FnKindU32>,
    clock: Option<FnKindU32>,
    max_clock: Option<FnKindU32>,
    /// (PCI bus, device handle)
    devices: Vec<(u32, Device)>,
}

/// NvThermalSensors (undocumented NvAPI_GPU_GetThermalSensors), as LibreHardwareMonitor uses it
#[repr(C)]
struct ThermalSensors {
    version: u32,
    mask: u32,
    reserved: [i32; 8],
    temperatures: [i32; 32],
}

type NvQuery = unsafe extern "C" fn(u32) -> *mut core::ffi::c_void;
type NvInit = unsafe extern "C" fn() -> i32;
type NvEnum = unsafe extern "C" fn(*mut [usize; 64], *mut u32) -> i32;
type NvBusId = unsafe extern "C" fn(usize, *mut u32) -> i32;
type NvSensors = unsafe extern "C" fn(usize, *mut ThermalSensors) -> i32;

struct NvApi {
    sensors: NvSensors,
    /// (PCI bus, physical GPU handle, sensor mask that answers)
    gpus: Vec<(u32, usize, u32)>,
}

pub struct Nvidia {
    nvml: Option<Nvml>,
    nvapi: Option<NvApi>,
}

fn load(name: PCWSTR) -> Option<HMODULE> {
    crate::util::load_system_dll(name)
}

unsafe fn sym<T: Copy>(lib: HMODULE, name: &[u8]) -> Option<T> {
    let f = GetProcAddress(lib, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&f))
}

impl Nvml {
    fn new() -> Option<Self> {
        // System32 on current drivers, Program Files\NVIDIA Corporation\NVSMI on older ones (the
        // Windows 7 drivers); ProgramW6432 is the 64-bit folder even for a 32-bit build
        let lib = load(windows::core::w!("nvml.dll")).or_else(|| {
            let root = std::env::var("ProgramW6432").or_else(|_| std::env::var("ProgramFiles")).ok()?;
            let path: Vec<u16> = format!(r"{root}\NVIDIA Corporation\NVSMI\nvml.dll").encode_utf16().chain(Some(0)).collect();
            unsafe { LoadLibraryW(PCWSTR(path.as_ptr())).ok() }
        })?;
        unsafe {
            let init: Fn0 = sym(lib, b"nvmlInit_v2\0")?;
            if init() != 0 {
                return None;
            }
            let count: FnCount = sym(lib, b"nvmlDeviceGetCount_v2\0")?;
            let handle: FnHandle = sym(lib, b"nvmlDeviceGetHandleByIndex_v2\0")?;
            let pci: FnPci = sym(lib, b"nvmlDeviceGetPciInfo_v3\0").or_else(|| sym(lib, b"nvmlDeviceGetPciInfo_v2\0"))?;
            let mut n = 0u32;
            if count(&mut n) != 0 {
                return None;
            }
            let mut devices = Vec::new();
            for i in 0..n.min(16) {
                let mut d: Device = std::ptr::null_mut();
                let mut info: PciInfo = std::mem::zeroed();
                if handle(i, &mut d) == 0 && pci(d, &mut info) == 0 {
                    devices.push((info.bus, d));
                }
            }
            Some(Self {
                power: sym(lib, b"nvmlDeviceGetPowerUsage\0"),
                limit: sym(lib, b"nvmlDeviceGetEnforcedPowerLimit\0"),
                gen: sym(lib, b"nvmlDeviceGetCurrPcieLinkGeneration\0"),
                width: sym(lib, b"nvmlDeviceGetCurrPcieLinkWidth\0"),
                max_gen: sym(lib, b"nvmlDeviceGetMaxPcieLinkGeneration\0"),
                max_width: sym(lib, b"nvmlDeviceGetMaxPcieLinkWidth\0"),
                fan: sym(lib, b"nvmlDeviceGetFanSpeed\0"),
                temp: sym(lib, b"nvmlDeviceGetTemperature\0"),
                clock: sym(lib, b"nvmlDeviceGetClockInfo\0"),
                max_clock: sym(lib, b"nvmlDeviceGetMaxClockInfo\0"),
                devices,
            })
        }
    }

    fn fill(&self, dev: Device, p: &mut Perf) {
        let get = |f: Option<FnU32>| -> Option<u32> {
            let f = f?;
            let mut v = 0u32;
            (unsafe { f(dev, &mut v) } == 0).then_some(v)
        };
        // the limit first: a card that just woke from D3 reports nonsense once (588 W in P8 on a
        // 4090 Laptop GPU limited to 80 W), and anything far above the enforced limit is that
        p.power_limit_w = get(self.limit).filter(|mw| *mw > 0 && *mw < 2_000_000).map(|mw| mw as f32 / 1000.0).or(p.power_limit_w);
        let cap_mw = p.power_limit_w.map_or(2_000_000, |w| (w * 1500.0) as u32);
        p.power_w = get(self.power).filter(|mw| *mw > 0 && *mw < 2_000_000 && *mw <= cap_mw).map(|mw| mw as f32 / 1000.0).or(p.power_w);
        p.pcie_gen = get(self.gen).filter(|g| (1..=7).contains(g)).or(p.pcie_gen);
        p.pcie_width = get(self.width).filter(|w| (1..=32).contains(w)).or(p.pcie_width);
        p.pcie_max_gen = get(self.max_gen).filter(|g| (1..=7).contains(g)).or(p.pcie_max_gen);
        p.pcie_max_width = get(self.max_width).filter(|w| (1..=32).contains(w)).or(p.pcie_max_width);
        p.fan_pct = get(self.fan).filter(|f| *f <= 100).or(p.fan_pct);
        // only where D3DKMT left them empty: it agrees with Task Manager
        let kind = |f: Option<FnKindU32>, k: u32| -> Option<u32> {
            let f = f?;
            let mut v = 0u32;
            (unsafe { f(dev, k, &mut v) } == 0).then_some(v)
        };
        if p.temp.is_none() {
            p.temp = kind(self.temp, NVML_TEMPERATURE_GPU).filter(|c| *c > 0 && *c < 150).map(|c| c as f32);
        }
        let valid = |m: &u32| *m > 0 && *m < 50_000;
        if p.core_mhz.is_none() {
            p.core_mhz = kind(self.clock, NVML_CLOCK_GRAPHICS).filter(valid);
            p.core_max_mhz = kind(self.max_clock, NVML_CLOCK_GRAPHICS).filter(valid);
        }
        if p.mem_mhz.is_none() {
            p.mem_mhz = kind(self.clock, NVML_CLOCK_MEM).filter(valid);
            p.mem_max_mhz = kind(self.max_clock, NVML_CLOCK_MEM).filter(valid);
        }
    }
}

impl NvApi {
    fn new() -> Option<Self> {
        #[cfg(target_pointer_width = "64")]
        let lib = load(windows::core::w!("nvapi64.dll"))?;
        #[cfg(not(target_pointer_width = "64"))]
        let lib = load(windows::core::w!("nvapi.dll"))?;
        unsafe {
            let query: NvQuery = sym(lib, b"nvapi_QueryInterface\0")?;
            let get = |id: u32| -> Option<*mut core::ffi::c_void> {
                let p = query(id);
                (!p.is_null()).then_some(p)
            };
            let init: NvInit = std::mem::transmute(get(0x0150E828)?);
            if init() != 0 {
                return None;
            }
            let enumerate: NvEnum = std::mem::transmute(get(0xE5AC921F)?);
            let bus_id: NvBusId = std::mem::transmute(get(0x1BE0B8E5)?);
            let sensors: NvSensors = std::mem::transmute(get(0x65FE3AAD)?);
            let mut handles = [0usize; 64];
            let mut n = 0u32;
            if enumerate(&mut handles, &mut n) != 0 {
                return None;
            }
            let mut gpus = Vec::new();
            for &h in handles.iter().take(n.min(64) as usize) {
                let mut bus = 0u32;
                if bus_id(h, &mut bus) != 0 {
                    continue;
                }
                // the highest sensor bit the GPU answers for, as LibreHardwareMonitor probes it
                let mut bits = 0u32;
                while bits < 32 {
                    let mut s = new_sensors(1u32 << bits);
                    if sensors(h, &mut s) != 0 {
                        break;
                    }
                    bits += 1;
                }
                if bits > 1 {
                    gpus.push((bus, h, ((1u64 << bits) - 1) as u32));
                }
            }
            if gpus.is_empty() {
                return None;
            }
            Some(Self { sensors, gpus })
        }
    }

    /// Hot spot: sensor 1 on every generation LibreHardwareMonitor knows except the RTX 50
    /// series, which reports no hot spot there.
    fn hotspot(&self, bus: u32, name: &str) -> Option<f32> {
        if name.contains("RTX 50") {
            return None;
        }
        let &(_, h, mask) = self.gpus.iter().find(|g| g.0 == bus)?;
        let mut s = new_sensors(mask);
        if unsafe { (self.sensors)(h, &mut s) } != 0 {
            return None;
        }
        let c = s.temperatures[1] as f32 / 256.0;
        (c > 0.0 && c < 150.0).then_some(c)
    }
}

fn new_sensors(mask: u32) -> ThermalSensors {
    let size = std::mem::size_of::<ThermalSensors>() as u32;
    ThermalSensors { version: size | (2 << 16), mask, reserved: [0; 8], temperatures: [0; 32] }
}

impl Nvidia {
    pub fn new() -> Self {
        Self { nvml: Nvml::new(), nvapi: NvApi::new() }
    }

    /// Add what the NVIDIA libraries know about the GPU on PCI `bus` (None: the only one).
    pub fn fill(&self, bus: Option<u32>, name: &str, p: &mut Perf) {
        if let Some(n) = &self.nvml {
            let dev = match bus {
                Some(b) => n.devices.iter().find(|d| d.0 == b).map(|d| d.1),
                None if n.devices.len() == 1 => Some(n.devices[0].1),
                None => None,
            };
            if let Some(d) = dev {
                n.fill(d, p);
            }
        }
        if let Some(a) = &self.nvapi {
            let bus = bus.or_else(|| (a.gpus.len() == 1).then(|| a.gpus[0].0));
            if let Some(b) = bus {
                p.hotspot = a.hotspot(b, name).or(p.hotspot);
            }
        }
    }
}
