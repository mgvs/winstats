pub mod amd;
pub mod battery;
pub mod cpu;
pub mod disks;
pub mod drives;
pub mod gpu;
pub mod gpuperf;
pub mod gpustats;
pub mod intel;
pub mod mem;
pub mod net;
pub mod nvidia;
pub mod ping;
pub mod pdh;
pub mod processes;
pub mod smbios;
pub mod sysinfo;

use crate::util::{pcwstr, wide};
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// samples kept per history chart: a stretched chart on a vertical taskbar shows a few minutes
pub const HISTORY: usize = 360;

/// Everything the widgets and the popups read: current values, histories, hardware lists.
/// Filled by the local collector (`Metrics`) or by a remote agent's snapshots; the drawing code
/// does not know which.
#[derive(Default, Clone)]
pub struct MetricsData {
    pub mem: mem::Mem,
    pub cpu_cores: Vec<f32>,
    pub cpu_total: f32,
    pub cpu_system: f32,
    pub cpu_user: f32,
    pub cpu_name: String,
    /// EfficiencyClass per displayed core; all equal on non-hybrid CPUs
    pub cpu_classes: Vec<u8>,
    /// package (socket) index per displayed core
    pub cpu_packages: Vec<u8>,
    /// true for a displayed core that is an SMT / Hyper-Threading sibling of an earlier one
    pub cpu_smt: Vec<bool>,
    /// OS index of each displayed core (display order may group P-cores first)
    pub cpu_ids: Vec<u16>,
    pub cpu_sockets: usize,
    /// model name per socket (local only; drawn when there are several)
    pub cpu_socket_names: Vec<String>,
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
    /// host the latency probe targets; empty = no probe (remote machines)
    pub net_ping_host: String,
    /// ms, from the ICMP probe while the popup is open
    pub net_latency: Option<f32>,
    pub net_jitter: Option<f32>,
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
    /// temperature, clocks and power of the primary adapter (popup only), where reported
    pub gpu_perf: gpuperf::Perf,
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
    /// GPU load and memory per app (popup only; Windows 10 1709+ GPU counters)
    pub top_gpu: Vec<processes::ProcInfo>,
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
    /// remote machine's name; empty for the local machine
    pub host: String,
    /// remote machine not reachable right now (its last values stay)
    pub offline: bool,
    /// CPU package temperature where the source has one
    pub cpu_temp: Option<f32>,
    /// current clock per displayed core, MHz; empty when unknown
    pub cpu_freq: Vec<u32>,
    /// 1, 5, 15 minute load averages (Unix agents)
    pub load_avg: Vec<f32>,
    /// machine fans an agent reads (Linux hwmon, Mac SMC); empty locally
    pub fans: Vec<Fan>,
    /// power rails / whole-machine power an agent reads (Jetson INA3221, Mac SMC); empty locally
    pub power: Vec<Power>,
    /// popup details; empty / None / 0 where unknown
    pub sys: SysDetails,
    /// a remote machine's system ("Ubuntu 24.04.3 LTS"), kernel, architecture and agent;
    /// empty for the local machine
    pub os: OsInfo,
}

#[derive(Default, Clone)]
pub struct OsInfo {
    pub release: String,
    pub kernel: String,
    pub arch: String,
    pub agent: String,
}

#[derive(Default, Clone)]
pub struct Power {
    pub id: String,
    /// W
    pub w: f32,
}

#[derive(Default, Clone)]
pub struct Fan {
    pub id: String,
    pub rpm: Option<u32>,
    /// PWM duty, 0..1
    pub duty: Option<f32>,
}

/// Extra popup rows, all local-only and read while a popup is open (the CPU facts once).
#[derive(Default, Clone)]
pub struct SysDetails {
    pub processes: u32,
    pub threads: u32,
    pub handles: u32,
    pub uptime_secs: u64,
    pub ctx_switches: Option<f64>,
    pub interrupts: Option<f64>,
    pub dpc_time: Option<f32>,
    pub interrupt_time: Option<f32>,
    pub perf_limit: Option<f32>,
    pub cpu: sysinfo::CpuInfo,
    pub pagefile_used: u64,
    pub pagefile_total: u64,
    pub hw_reserved: u64,
}

impl MetricsData {
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

    /// Average current clock in MHz over the displayed cores of `class` (None = all cores).
    pub fn cpu_clock(&self, class: Option<u8>) -> Option<u32> {
        let mut sum = 0u64;
        let mut n = 0u64;
        for (i, f) in self.cpu_freq.iter().enumerate() {
            if *f == 0 || class.map_or(false, |c| self.cpu_classes.get(i).cloned().unwrap_or(0) != c) {
                continue;
            }
            sum += *f as u64;
            n += 1;
        }
        if n > 0 {
            Some((sum / n) as u32)
        } else {
            None
        }
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

/// The local machine: Windows collectors plus the `MetricsData` they fill. Derefs to the data
/// so drawing code takes `&MetricsData` and callers pass `&Metrics` unchanged.
pub struct Metrics {
    pub data: MetricsData,
    cpu: cpu::Cpu,
    net: net::Net,
    pdh: Option<pdh::Pdh>,
    gpu_dev: gpu::Gpu,
    /// GPU load from D3DKMT statistics where the GPU Engine counters are missing (Windows 7)
    gpu_stats: Option<gpustats::GpuStatsWorker>,
    procs: processes::Processes,
    disk_letter: String,
    /// Display order: hardware index of each displayed core (P-cores first, then E-cores).
    /// Windows numbers cores in die order, which interleaves P and E clusters on Arrow Lake.
    cpu_order: Vec<usize>,
    /// read the per-disk counters every tick (sharing needs them), not only for the popup
    pub want_per_disk: bool,
    /// the local GPU popup is open: read the GPU sensors, processes and vendor libraries
    pub gpu_details: bool,
    /// popup-only counters; opened the first time a popup shows details
    details: Option<pdh::Details>,
    /// the details query could not be opened: do not retry every tick
    details_failed: bool,
    /// last disk temperature read: disks are asked every few seconds only
    disk_temp_at: Option<std::time::Instant>,
    /// process lists refresh on their own, slower clock (config `proc_update_ms`) so the rows
    /// stay readable while the charts move fast
    pub proc_interval_ms: u32,
    proc_at: Option<std::time::Instant>,
    /// the process lists were just refreshed: the GPU list follows on this tick
    procs_fresh: bool,
    /// load history of every adapter by LUID: switching adapters in the popup shows the chosen
    /// one's own past instead of continuing the previous one's line
    gpu_histories: std::collections::HashMap<u64, VecDeque<f32>>,
}

impl Deref for Metrics {
    type Target = MetricsData;
    fn deref(&self) -> &MetricsData {
        &self.data
    }
}

impl DerefMut for Metrics {
    fn deref_mut(&mut self) -> &mut MetricsData {
        &mut self.data
    }
}

impl Metrics {
    pub fn new(disk_letter: &str, top_n: usize, net_disabled: &[String], disks_disabled: &[String]) -> Self {
        let cpu = cpu::Cpu::new();
        let cores = cpu.cores.len();
        let gpu_dev = gpu::Gpu::new(&[]);
        let pdh = pdh::Pdh::new();
        // WINSTATS_WIN7_GPU: take the Windows 7 path (no GPU Engine counters) for testing
        let pdh_gpu = pdh.as_ref().map(|p| p.gpu_available()).unwrap_or(false) && std::env::var_os("WINSTATS_WIN7_GPU").is_none();
        let gpu_stats = if pdh_gpu { None } else { gpustats::GpuStats::new(&gpu_dev.adapters.iter().map(|a| a.luid).collect::<Vec<_>>()).map(gpustats::GpuStatsWorker::start) };
        if gpu_stats.is_some() {
            crate::log!("GPU load from D3DKMT statistics");
        }
        let gpu_available = pdh_gpu || gpu_stats.is_some();
        let battery_available = battery::read().is_some();
        let cpu_order: Vec<usize> = (0..cores).collect();
        let mem_modules = smbios::memory_modules();
        let mem_summary = smbios::summary(&mem_modules);
        let mem_channels = smbios::channels(&mem_modules);
        let data = MetricsData {
            cpu_name: cpu.name.clone(),
            cpu_classes: cpu.classes.clone(),
            cpu_packages: cpu.packages.clone(),
            cpu_smt: cpu.smt.clone(),
            cpu_ids: (0..cores as u16).collect(),
            cpu_sockets: cpu.packages.iter().cloned().max().map(|m| m as usize + 1).unwrap_or(1),
            cpu_socket_names: cpu.socket_names.clone(),
            gpu_name: gpu_dev.name.clone(),
            gpu_adapters: gpu_dev.adapters.clone(),
            gpu_primary: gpu_dev.primary().cloned(),
            disks: disks::with_disabled(disks::enumerate(), disks_disabled),
            mem_modules,
            mem_summary,
            mem_channels,
            vram_total: gpu_dev.vram_total,
            top_n: top_n.max(1),
            sys: SysDetails { cpu: sysinfo::cpu_info(), ..Default::default() },
            gpu_available,
            battery_available,
            ..Default::default()
        };
        Self {
            data,
            cpu,
            net: net::Net::new(net_disabled),
            pdh,
            gpu_dev,
            gpu_stats,
            procs: processes::Processes::new(cores, top_n),
            disk_letter: disk_letter.to_string(),
            cpu_order,
            want_per_disk: false,
            gpu_details: false,
            details: None,
            details_failed: false,
            disk_temp_at: None,
            proc_interval_ms: 2000,
            proc_at: None,
            procs_fresh: false,
            gpu_histories: Default::default(),
        }
    }

    /// Display order of cores: hardware order, or P-cores first then E-cores.
    pub fn set_group_pe(&mut self, group: bool) {
        let n = self.cpu.cores.len();
        self.cpu_order = (0..n).collect();
        if group {
            // socket first, then P-cores before E-cores within it
            let cpu = &self.cpu;
            self.cpu_order.sort_by_key(|&i| (cpu.packages.get(i).cloned().unwrap_or(0), std::cmp::Reverse(cpu.classes.get(i).cloned().unwrap_or(0)), i));
        }
        self.data.cpu_classes = self.cpu_order.iter().map(|&i| self.cpu.classes.get(i).cloned().unwrap_or(0)).collect();
        self.data.cpu_packages = self.cpu_order.iter().map(|&i| self.cpu.packages.get(i).cloned().unwrap_or(0)).collect();
        self.data.cpu_smt = self.cpu_order.iter().map(|&i| self.cpu.smt.get(i).cloned().unwrap_or(false)).collect();
        self.data.cpu_ids = self.cpu_order.iter().map(|&i| i as u16).collect();
        self.data.cpu_cores = self.cpu_order.iter().map(|&i| self.cpu.cores.get(i).cloned().unwrap_or(0.0)).collect();
    }

    pub fn set_net_disabled(&mut self, disabled: &[String]) {
        self.net.set_disabled(disabled);
        self.data.net_ifaces = self.net.ifaces.clone();
    }

    pub fn set_gpu_disabled(&mut self, disabled: &[String]) {
        self.gpu_dev.set_disabled(disabled);
        let d = &mut self.data;
        d.gpu_name = self.gpu_dev.name.clone();
        d.gpu_adapters = self.gpu_dev.adapters.clone();
        d.gpu_primary = self.gpu_dev.primary().cloned();
        d.gpu_perf = self.gpu_dev.primary().map(|a| a.perf).unwrap_or_default();
        d.vram_total = self.gpu_dev.vram_total;
        d.vram_used = self.gpu_dev.vram_used;
        d.vram_budget = self.gpu_dev.vram_budget;
        if let Some(h) = d.gpu_primary.as_ref().and_then(|p| self.gpu_histories.get(&p.luid)) {
            d.gpu_history = h.clone();
        }
    }

    pub fn set_disks_disabled(&mut self, disabled: &[String]) {
        self.data.disks = disks::with_disabled(std::mem::take(&mut self.data.disks), disabled);
    }

    pub fn set_top_n(&mut self, n: usize) {
        self.procs.top_n = n.max(1);
        self.data.top_n = self.procs.top_n;
    }

    /// Called when the popup opens so the first per-process sample is not garbage.
    pub fn begin_details(&mut self) {
        self.procs.reset();
        self.procs.update();
        // the first sample has no rates yet: the next tick refreshes the lists at once
        self.proc_at = None;
        self.update_details();
    }

    /// SYS / USER filter of the process lists; the next tick rebuilds them.
    pub fn set_proc_filter(&mut self, system: bool, user: bool) {
        self.procs.show_system = system;
        self.procs.show_user = user;
        self.proc_at = None;
    }

    /// Rebuild the process lists now (after a filter change), without a new chart sample.
    pub fn refresh_processes(&mut self) {
        self.procs.update();
        self.proc_at = Some(std::time::Instant::now());
        let d = &mut self.data;
        d.top_cpu = self.procs.top_cpu.clone();
        d.top_mem = self.procs.top_mem.clone();
        d.top_io = self.procs.top_io.clone();
        // the GPU list follows on the next tick
        d.top_gpu.clear();
        self.procs_fresh = false;
        self.proc_at = None;
    }

    /// The interfaces changed (adapter plugged / unplugged, link up / down): re-sample now
    /// rather than on the next tick.
    pub fn refresh_net(&mut self) {
        self.net.update();
        self.net.update_details();
        let d = &mut self.data;
        d.net_iface = self.net.iface_name.clone();
        d.net_ip = self.net.iface_ip.clone();
        d.net_ifaces = self.net.ifaces.clone();
    }

    fn update_details(&mut self) {
        if self.details.is_none() && !self.details_failed {
            self.details = pdh::Details::new();
            self.details_failed = self.details.is_none();
        }
        let sample = self.details.as_mut().map(|x| x.update()).unwrap_or_default();
        let d = &mut self.data;
        d.sys.uptime_secs = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() } / 1000;
        d.sys.ctx_switches = sample.ctx_switches;
        d.sys.interrupts = sample.interrupts;
        d.sys.dpc_time = sample.dpc_time;
        d.sys.interrupt_time = sample.interrupt_time;
        d.sys.perf_limit = sample.perf_limit;
        d.cpu_temp = sample.zone_temp;
        (d.sys.pagefile_used, d.sys.pagefile_total) = sysinfo::page_file();
        if d.sys.hw_reserved == 0 && d.mem.total > 0 {
            d.sys.hw_reserved = sysinfo::hardware_reserved(d.mem.total);
        }
        if self.disk_temp_at.map_or(true, |t| t.elapsed().as_secs() >= 10) {
            self.disk_temp_at = Some(std::time::Instant::now());
            for disk in d.disks.iter_mut() {
                disk.temp = disks::temperature(disk.index);
            }
        }
        for (index, busy, queue, ms, iops) in &sample.disks {
            if let Some(disk) = d.disks.iter_mut().find(|disk| disk.index == *index) {
                disk.active = Some(*busy);
                disk.queue = Some(*queue);
                disk.latency_ms = Some(*ms);
                disk.iops = Some(*iops);
            }
        }
        if self.gpu_details {
            if self.procs_fresh {
                let filter = (self.procs.show_system, self.procs.show_user);
                let load = match (self.gpu_stats.as_mut(), self.pdh.as_ref()) {
                    (Some(st), _) => Some(st.by_pid(self.procs.names.keys().cloned().collect())),
                    (None, Some(p)) => Some(p.gpu_by_pid()),
                    _ => None,
                };
                if let Some(load) = load {
                    d.top_gpu = processes::top_gpu(&load, &sample.gpu_proc_mem, &self.procs.names, filter, d.top_n);
                }
            }
            self.gpu_dev.set_adapter_mem(&sample.gpu_adapter_mem);
        }
        self.net.update_details();
        d.net_iface = self.net.iface_name.clone();
        d.net_ip = self.net.iface_ip.clone();
        d.net_ifaces = self.net.ifaces.clone();
        if self.gpu_details {
            // DXGI usage wakes a sleeping Optimus GPU and takes hundreds of ms: only for the GPU
            // popup, and only where the adapter memory counters are missing
            if self.gpu_dev.primary().map_or(false, |a| a.dedicated_used.is_none()) {
                self.gpu_dev.update();
            }
            self.gpu_dev.update_perf();
        } else {
            self.gpu_dev.stop_perf();
        }
        d.gpu_perf = self.gpu_dev.primary().map(|a| a.perf).unwrap_or_default();
        d.gpu_adapters = self.gpu_dev.adapters.clone();
        d.vram_used = self.gpu_dev.vram_used;
        d.vram_budget = self.gpu_dev.vram_budget;
        d.drives = drives::list();
        d.battery_detail = battery::detail();
        if let Some(p) = self.pdh.as_ref() {
            for (index, r, w) in p.per_disk() {
                if let Some(disk) = d.disks.iter_mut().find(|disk| disk.index == index) {
                    disk.read_bps = r;
                    disk.write_bps = w;
                }
            }
        }
    }

    pub fn update(&mut self, details: bool) {
        let d = &mut self.data;
        self.cpu.update();
        d.cpu_cores = self.cpu_order.iter().map(|&i| self.cpu.cores.get(i).cloned().unwrap_or(0.0)).collect();
        d.cpu_total = self.cpu.total;
        d.cpu_system = self.cpu.system;
        d.cpu_user = self.cpu.user;
        push(&mut d.cpu_history, d.cpu_total);

        d.mem.update();
        push(&mut d.mem_history, d.mem.pct);

        self.net.update();
        d.net_rx = self.net.rx_bps;
        d.net_tx = self.net.tx_bps;
        d.net_total_rx = self.net.total_rx;
        d.net_total_tx = self.net.total_tx;
        d.net_ifaces = self.net.ifaces.clone();
        push(&mut d.net_history, (d.net_rx as f32, d.net_tx as f32));

        if let Some(st) = self.gpu_stats.as_ref() {
            if !details || !self.gpu_details {
                st.by_pid(Vec::new());
            }
            d.gpu = self.gpu_dev.set_loads(&st.loads());
            d.gpu_adapters = self.gpu_dev.adapters.clone();
        }
        if let Some(p) = self.pdh.as_mut() {
            p.update();
            if self.gpu_stats.is_none() {
                d.gpu = self.gpu_dev.set_loads(&p.gpu_loads);
                d.gpu_adapters = self.gpu_dev.adapters.clone();
            }
            let mut raw = p.core_freqs();
            if raw.is_empty() {
                raw = sysinfo::current_clocks();
            }
            d.cpu_freq = if raw.len() >= self.cpu_order.len() { self.cpu_order.iter().map(|&i| raw.get(i).cloned().unwrap_or(0)).collect() } else { Vec::new() };
            if self.want_per_disk || d.disks.iter().any(|disk| !disk.enabled) {
                // some disks are switched off: sum the per-disk counters instead of _Total
                let (mut r, mut w) = (0.0, 0.0);
                for (index, dr, dw) in p.per_disk() {
                    if let Some(disk) = d.disks.iter_mut().find(|disk| disk.index == index) {
                        disk.read_bps = dr;
                        disk.write_bps = dw;
                        if disk.enabled {
                            r += dr;
                            w += dw;
                        }
                    }
                }
                d.disk_read = r;
                d.disk_write = w;
            } else {
                d.disk_read = p.disk_read_bps;
                d.disk_write = p.disk_write_bps;
            }
        }
        push(&mut d.disk_history, (d.disk_read as f32, d.disk_write as f32));
        // the shown adapter's line gets what the widget shows, every other adapter its own load
        let primary = self.gpu_dev.primary().map(|a| a.luid);
        for a in &self.gpu_dev.adapters {
            let v = if Some(a.luid) == primary { d.gpu } else { a.load };
            push(self.gpu_histories.entry(a.luid).or_default(), v.unwrap_or(0.0));
        }
        match primary.and_then(|l| self.gpu_histories.get(&l)) {
            Some(h) => d.gpu_history = h.clone(),
            None => push(&mut d.gpu_history, d.gpu.unwrap_or(0.0)),
        }

        let path = wide(&format!("{}\\", self.disk_letter.trim_end_matches('\\')));
        let mut free = 0u64;
        let mut total = 0u64;
        if unsafe { GetDiskFreeSpaceExW(pcwstr(&path), None, Some(&mut total), Some(&mut free)) }.is_ok() {
            d.disk_free = free;
            d.disk_total = total;
        }

        d.battery = battery::read();
        if !details || !self.gpu_details {
            // the sensor thread reads only while the GPU popup is open
            self.gpu_dev.stop_perf();
        }

        if details {
            let due = self.proc_at.map_or(true, |t| t.elapsed().as_millis() as u64 + 50 >= self.proc_interval_ms as u64);
            self.procs_fresh = due;
            if due {
                self.proc_at = Some(std::time::Instant::now());
                self.procs.update();
                d.top_cpu = self.procs.top_cpu.clone();
                d.top_mem = self.procs.top_mem.clone();
                d.top_io = self.procs.top_io.clone();
                d.mem_processes = self.procs.mem_total;
            }
            unsafe {
                use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
                let mut pi = PERFORMANCE_INFORMATION { cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
                if GetPerformanceInfo(&mut pi, pi.cb).is_ok() {
                    d.sys.processes = pi.ProcessCount;
                    d.sys.threads = pi.ThreadCount;
                    d.sys.handles = pi.HandleCount;
                    let page = pi.PageSize as u64;
                    d.mem_kernel_paged = pi.KernelPaged as u64 * page;
                    d.mem_kernel_nonpaged = pi.KernelNonpaged as u64 * page;
                    d.mem_cache = pi.SystemCache as u64 * page;
                }
            }
            self.update_details();
        }
    }
}

/// Append to a bounded history.
pub fn push<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() >= HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}
