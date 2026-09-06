pub mod battery;
pub mod cpu;
pub mod disks;
pub mod drives;
pub mod gpu;
pub mod mem;
pub mod net;
pub mod pdh;
pub mod processes;
pub mod smbios;

use crate::util::{pcwstr, wide};
use std::collections::VecDeque;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

pub const HISTORY: usize = 120;

pub struct Metrics {
    cpu: cpu::Cpu,
    pub mem: mem::Mem,
    net: net::Net,
    pdh: Option<pdh::Pdh>,
    gpu_dev: gpu::Gpu,
    procs: processes::Processes,
    disk_letter: String,

    pub cpu_cores: Vec<f32>,
    pub cpu_total: f32,
    pub cpu_system: f32,
    pub cpu_user: f32,
    pub cpu_name: String,
    /// EfficiencyClass per displayed core; all equal on non-hybrid CPUs
    pub cpu_classes: Vec<u8>,
    /// package (socket) index per displayed core
    pub cpu_packages: Vec<u8>,
    pub cpu_sockets: usize,
    /// Display order: hardware index of each displayed core (P-cores first, then E-cores).
    /// Windows numbers cores in die order, which interleaves P and E clusters on Arrow Lake.
    cpu_order: Vec<usize>,
    pub cpu_history: VecDeque<f32>,
    pub mem_history: VecDeque<f32>,
    pub net_rx: f64,
    pub net_tx: f64,
    pub net_total_rx: u64,
    pub net_total_tx: u64,
    pub net_iface: String,
    pub net_ip: String,
    /// every hardware interface with its own rate (refreshed while the popup is open)
    pub net_ifaces: Vec<net::NetIface>,
    pub net_history: VecDeque<(f32, f32)>,
    pub disk_read: f64,
    pub disk_write: f64,
    pub disk_history: VecDeque<(f32, f32)>,
    pub disk_free: u64,
    pub disk_total: u64,
    pub drives: Vec<drives::Drive>,
    pub gpu: Option<f32>,
    pub gpu_history: VecDeque<f32>,
    pub gpu_name: String,
    pub gpu_adapters: Vec<gpu::AdapterInfo>,
    pub gpu_primary: Option<gpu::AdapterInfo>,
    /// physical disks with model/bus/size; rates filled while the popup is open
    pub disks: Vec<disks::PhysicalDisk>,
    pub mem_modules: Vec<smbios::MemModule>,
    pub mem_summary: Option<String>,
    /// memory channels in use, from the module locators in SMBIOS
    pub mem_channels: Option<usize>,
    pub vram_used: u64,
    pub vram_budget: u64,
    pub vram_total: u64,
    pub battery: Option<battery::Battery>,
    pub battery_detail: Option<battery::BatteryDetail>,
    pub top_cpu: Vec<processes::ProcInfo>,
    pub top_mem: Vec<processes::ProcInfo>,
    pub top_io: Vec<processes::ProcInfo>,
    pub top_n: usize,
    /// memory breakdown for the popup (details only)
    pub mem_processes: u64,
    pub mem_kernel_paged: u64,
    pub mem_kernel_nonpaged: u64,
    pub mem_cache: u64,
    /// false when the PDH "GPU Engine" counters do not exist (pre-Windows 10 or no driver support)
    pub gpu_available: bool,
    /// false on machines without a battery
    pub battery_available: bool,
}

impl Metrics {
    pub fn new(disk_letter: &str, top_n: usize, net_disabled: &[String], disks_disabled: &[String]) -> Self {
        let cpu = cpu::Cpu::new();
        let cores = cpu.cores.len();
        let gpu_dev = gpu::Gpu::new();
        let pdh = pdh::Pdh::new();
        let gpu_available = pdh.as_ref().map(|p| p.gpu_available()).unwrap_or(false);
        let battery_available = battery::read().is_some();
        let cpu_order: Vec<usize> = (0..cores).collect();
        let cpu_classes = cpu.classes.clone();
        let cpu_packages = cpu.packages.clone();
        let cpu_sockets = cpu.packages.iter().cloned().max().map(|m| m as usize + 1).unwrap_or(1);
        let mem_modules = smbios::memory_modules();
        let mem_summary = smbios::summary(&mem_modules);
        let mem_channels = smbios::channels(&mem_modules);
        Self {
            cpu_name: cpu.name.clone(),
            cpu_classes,
            cpu_packages,
            cpu_sockets,
            cpu_order,
            gpu_name: gpu_dev.name.clone(),
            gpu_adapters: gpu_dev.adapters.clone(),
            gpu_primary: gpu_dev.primary().cloned(),
            disks: disks::with_disabled(disks::enumerate(), disks_disabled),
            mem_modules,
            mem_summary,
            mem_channels,
            vram_total: gpu_dev.vram_total,
            cpu,
            mem: mem::Mem::default(),
            net: net::Net::new(net_disabled),
            pdh,
            gpu_dev,
            procs: processes::Processes::new(cores, top_n),
            disk_letter: disk_letter.to_string(),
            cpu_cores: Vec::new(),
            cpu_total: 0.0,
            cpu_system: 0.0,
            cpu_user: 0.0,
            cpu_history: VecDeque::with_capacity(HISTORY),
            mem_history: VecDeque::with_capacity(HISTORY),
            net_rx: 0.0,
            net_tx: 0.0,
            net_total_rx: 0,
            net_total_tx: 0,
            net_iface: String::new(),
            net_ip: String::new(),
            net_ifaces: Vec::new(),
            net_history: VecDeque::with_capacity(HISTORY),
            disk_read: 0.0,
            disk_write: 0.0,
            disk_history: VecDeque::with_capacity(HISTORY),
            disk_free: 0,
            disk_total: 0,
            drives: Vec::new(),
            gpu: None,
            gpu_history: VecDeque::with_capacity(HISTORY),
            vram_used: 0,
            vram_budget: 0,
            battery: None,
            battery_detail: None,
            top_cpu: Vec::new(),
            top_mem: Vec::new(),
            top_io: Vec::new(),
            top_n: top_n.max(1),
            mem_processes: 0,
            mem_kernel_paged: 0,
            mem_kernel_nonpaged: 0,
            mem_cache: 0,
            gpu_available,
            battery_available,
        }
    }

    /// Display order of cores: hardware order, or P-cores first then E-cores.
    pub fn set_group_pe(&mut self, group: bool) {
        let n = self.cpu.cores.len();
        self.cpu_order = (0..n).collect();
        if group {
            // socket first, then P-cores before E-cores within it
            self.cpu_order.sort_by_key(|&i| (self.cpu.packages.get(i).cloned().unwrap_or(0), std::cmp::Reverse(self.cpu.classes.get(i).cloned().unwrap_or(0)), i));
        }
        self.cpu_classes = self.cpu_order.iter().map(|&i| self.cpu.classes.get(i).cloned().unwrap_or(0)).collect();
        self.cpu_packages = self.cpu_order.iter().map(|&i| self.cpu.packages.get(i).cloned().unwrap_or(0)).collect();
        self.cpu_cores = self.cpu_order.iter().map(|&i| self.cpu.cores.get(i).cloned().unwrap_or(0.0)).collect();
    }

    pub fn set_net_disabled(&mut self, disabled: &[String]) {
        self.net.set_disabled(disabled);
        self.net_ifaces = self.net.ifaces.clone();
    }

    pub fn set_disks_disabled(&mut self, disabled: &[String]) {
        self.disks = disks::with_disabled(std::mem::take(&mut self.disks), disabled);
    }

    pub fn set_top_n(&mut self, n: usize) {
        self.procs.top_n = n.max(1);
        self.top_n = self.procs.top_n;
    }

    /// Called when the popup opens so the first per-process sample is not garbage.
    pub fn begin_details(&mut self) {
        self.procs.reset();
        self.procs.update();
        self.update_details();
    }

    fn update_details(&mut self) {
        self.net.update_details();
        self.net_iface = self.net.iface_name.clone();
        self.net_ip = self.net.iface_ip.clone();
        self.net_ifaces = self.net.ifaces.clone();
        self.gpu_dev.update();
        self.vram_used = self.gpu_dev.vram_used;
        self.vram_budget = self.gpu_dev.vram_budget;
        self.drives = drives::list();
        self.battery_detail = battery::detail();
        if let Some(p) = self.pdh.as_ref() {
            for (index, r, w) in p.per_disk() {
                if let Some(d) = self.disks.iter_mut().find(|d| d.index == index) {
                    d.read_bps = r;
                    d.write_bps = w;
                }
            }
        }
    }

    pub fn update(&mut self, details: bool) {
        self.cpu.update();
        self.cpu_cores = self.cpu_order.iter().map(|&i| self.cpu.cores.get(i).cloned().unwrap_or(0.0)).collect();
        self.cpu_total = self.cpu.total;
        self.cpu_system = self.cpu.system;
        self.cpu_user = self.cpu.user;
        push(&mut self.cpu_history, self.cpu_total);

        self.mem.update();
        push(&mut self.mem_history, self.mem.pct);

        self.net.update();
        self.net_rx = self.net.rx_bps;
        self.net_tx = self.net.tx_bps;
        self.net_total_rx = self.net.total_rx;
        self.net_total_tx = self.net.total_tx;
        push(&mut self.net_history, (self.net_rx as f32, self.net_tx as f32));

        if let Some(p) = self.pdh.as_mut() {
            p.update();
            self.gpu = p.gpu_pct;
            if self.disks.iter().any(|d| !d.enabled) {
                // some disks are switched off: sum the per-disk counters instead of _Total
                let (mut r, mut w) = (0.0, 0.0);
                for (index, dr, dw) in p.per_disk() {
                    if let Some(d) = self.disks.iter_mut().find(|d| d.index == index) {
                        d.read_bps = dr;
                        d.write_bps = dw;
                        if d.enabled {
                            r += dr;
                            w += dw;
                        }
                    }
                }
                self.disk_read = r;
                self.disk_write = w;
            } else {
                self.disk_read = p.disk_read_bps;
                self.disk_write = p.disk_write_bps;
            }
        }
        push(&mut self.disk_history, (self.disk_read as f32, self.disk_write as f32));
        push(&mut self.gpu_history, self.gpu.unwrap_or(0.0));

        let path = wide(&format!("{}\\", self.disk_letter.trim_end_matches('\\')));
        let mut free = 0u64;
        let mut total = 0u64;
        if unsafe { GetDiskFreeSpaceExW(pcwstr(&path), None, Some(&mut total), Some(&mut free)) }.is_ok() {
            self.disk_free = free;
            self.disk_total = total;
        }

        self.battery = battery::read();

        if details {
            self.procs.update();
            self.top_cpu = self.procs.top_cpu.clone();
            self.top_mem = self.procs.top_mem.clone();
            self.top_io = self.procs.top_io.clone();
            self.mem_processes = self.procs.mem_total;
            unsafe {
                use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
                let mut pi = PERFORMANCE_INFORMATION { cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
                if GetPerformanceInfo(&mut pi, pi.cb).is_ok() {
                    let page = pi.PageSize as u64;
                    self.mem_kernel_paged = pi.KernelPaged as u64 * page;
                    self.mem_kernel_nonpaged = pi.KernelNonpaged as u64 * page;
                    self.mem_cache = pi.SystemCache as u64 * page;
                }
            }
            self.update_details();
        }
    }
}

impl Metrics {
    /// Where to draw a divider in the core charts: before core i when its socket or its
    /// efficiency class differs from core i-1.
    pub fn cpu_breaks(&self) -> Vec<bool> {
        (0..self.cpu_cores.len())
            .map(|i| {
                i > 0
                    && (self.cpu_classes.get(i) != self.cpu_classes.get(i - 1)
                        || self.cpu_packages.get(i) != self.cpu_packages.get(i - 1))
            })
            .collect()
    }

    /// True when the CPU has cores of different efficiency classes (P/E).
    pub fn cpu_hybrid(&self) -> bool {
        self.cpu_classes.iter().any(|c| *c != self.cpu_classes[0])
    }

    /// (P-core count, E-core count, P load, E load). P = highest efficiency class.
    pub fn cpu_pe(&self) -> (usize, usize, f32, f32) {
        let top = self.cpu_classes.iter().cloned().max().unwrap_or(0);
        let mut p = (0usize, 0f32);
        let mut e = (0usize, 0f32);
        for (i, v) in self.cpu_cores.iter().enumerate() {
            if self.cpu_classes.get(i).cloned().unwrap_or(top) == top {
                p.0 += 1;
                p.1 += v;
            } else {
                e.0 += 1;
                e.1 += v;
            }
        }
        (p.0, e.0, if p.0 > 0 { p.1 / p.0 as f32 } else { 0.0 }, if e.0 > 0 { e.1 / e.0 as f32 } else { 0.0 })
    }
}

fn push<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() >= HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}
