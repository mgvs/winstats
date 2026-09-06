//! GPU identity and VRAM usage through DXGI: every hardware adapter with its vendor/device id,
//! dedicated memory and user-mode driver version; VRAM usage for the primary one (DXGI 1.4).

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
    adapter: Option<IDXGIAdapter3>,
    pub name: String,
    pub vram_total: u64,
    pub vram_used: u64,
    pub vram_budget: u64,
    pub adapters: Vec<AdapterInfo>,
}

impl Gpu {
    pub fn new() -> Self {
        let mut g = Self { adapter: None, name: String::new(), vram_total: 0, vram_used: 0, vram_budget: 0, adapters: Vec::new() };
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return g };
            let mut i = 0;
            let mut best: Option<(u64, IDXGIAdapter1, String)> = None;
            while let Ok(a) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = a.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').trim().to_string();
                let driver = a
                    .CheckInterfaceSupport(&IDXGIDevice::IID)
                    .map(|v| {
                        let v = v as u64;
                        format!("{}.{}.{}.{}", (v >> 48) & 0xFFFF, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
                    })
                    .unwrap_or_default();
                g.adapters.push(AdapterInfo {
                    name: name.clone(),
                    vendor_id: desc.VendorId,
                    device_id: desc.DeviceId,
                    dedicated: desc.DedicatedVideoMemory as u64,
                    driver,
                });
                if best.as_ref().map(|b| desc.DedicatedVideoMemory as u64 > b.0).unwrap_or(true) {
                    best = Some((desc.DedicatedVideoMemory as u64, a, name));
                }
            }
            if let Some((mem, a, name)) = best {
                g.name = name;
                g.vram_total = mem;
                g.adapter = a.cast::<IDXGIAdapter3>().ok();
            }
        }
        g
    }

    /// The adapter the widgets describe (the one with the most dedicated memory).
    pub fn primary(&self) -> Option<&AdapterInfo> {
        self.adapters.iter().find(|a| a.name == self.name)
    }

    pub fn update(&mut self) {
        let Some(a) = &self.adapter else { return };
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        if unsafe { a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }.is_ok() {
            self.vram_used = info.CurrentUsage;
            self.vram_budget = info.Budget;
        }
    }
}
