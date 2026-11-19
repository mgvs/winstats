//! Metrics through `sysinfo`: one refresh per interval into a `Snapshot`, and heavier
//! per-process / per-mount material on demand for `Details`.

use crate::linux::Root;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use sysinfo::{Components, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};
use winstats_proto::*;

pub struct Collector {
    sys: System,
    nets: Networks,
    disks: Disks,
    comps: Components,
    /// thermal zones, added to what `comps` (hwmon) reports
    zones: Vec<(String, std::path::PathBuf)>,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    gpu: Option<crate::linux::gpu::Gpu>,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fans: crate::linux::hwmon::Fans,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    rails: crate::linux::hwmon::Rails,
    #[cfg(target_os = "macos")]
    mac: crate::macos::Mac,
    last: Instant,
    seq: u64,
    /// process table: refreshed only when a master asks for details
    procs: Option<System>,
    procs_at: Option<Instant>,
    procs_prev_io: HashMap<u32, (u64, u64, Instant)>,
    #[cfg(windows)]
    wincpu: crate::wincpu::WinCpu,
    #[cfg(not(windows))]
    unixcpu: crate::unixcpu::UnixCpu,
    /// last cpu fraction, for the beacon
    last_cpu: f32,
    pub info: Info,
    root: Root,
    disk_src: Vec<DiskSrc>,
    diskstats: crate::linux::block::DiskStats,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    syscnt: crate::linux::sys::SysCounters,
    wifi: crate::linux::net::Wifi,
    lists_at: Instant,
}

/// Interfaces that are not hardware: loopback, containers, VPNs, bridges.
fn skip_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "lo"
        || n.starts_with("lo0")
        || n.starts_with("docker")
        || n.starts_with("veth")
        || n.starts_with("br-")
        || n.starts_with("virbr")
        || n.starts_with("tun")
        || n.starts_with("tap")
        || n.starts_with("utun")
        || n.starts_with("tailscale")
        || n.starts_with("awdl")
        || n.starts_with("llw")
        || n.starts_with("bridge")
        || n.starts_with("anpi")
        || n.starts_with("ap1")
        || n.starts_with("gif")
        || n.starts_with("stf")
        || n.starts_with("xhc")
        || n.starts_with("vmnet")
        || n.starts_with("zt")
        || n.starts_with("wg")
        || n.starts_with("cni")
        || n.starts_with("flannel")
        || n.starts_with("kube")
        || n.contains("loopback")
        || n.contains("vethernet")
        || n.contains("hyper-v")
        || n.contains("vmware")
        || n.contains("virtualbox")
}

fn iface_kind(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.starts_with("wl") || n.contains("wi-fi") || n.contains("wifi") || n.contains("wlan") {
        "Wi-Fi"
    } else if n.starts_with("ww") || n.contains("mobile") || n.contains("cellular") {
        "Mobile"
    } else if n.contains("bluetooth") || n.starts_with("bnep") {
        "Bluetooth"
    } else if n.starts_with("en") || n.starts_with("eth") || n.contains("ethernet") {
        "Ethernet"
    } else {
        ""
    }
}

/// Mounted file systems that are not storage: snaps, overlays, ram disks.
fn skip_mount(fs: &str, name: &str, mount: &str) -> bool {
    let fs = fs.to_ascii_lowercase();
    matches!(fs.as_str(), "squashfs" | "overlay" | "tmpfs" | "devtmpfs" | "ramfs" | "proc" | "sysfs" | "cgroup" | "cgroup2" | "efivarfs" | "fuse.portal" | "autofs" | "devfs" | "nullfs")
        || name.starts_with("/dev/loop")
        || mount.starts_with("/snap/")
        || mount.starts_with("/boot/efi")
        || mount.starts_with("/System/Volumes/")
        || mount.starts_with("/private/var/vm")
        || mount.starts_with("/dev")
        || mount.starts_with("/run/")
        || mount.starts_with("/sys/")
        || mount.starts_with("/proc/")
}

/// Device name of a mounted volume ("/dev/nvme0n1p2", "disk1s1"); Windows reports none, so the
/// mount point ("C:\\") stands in.
fn disk_id(d: &sysinfo::Disk, mount: &str) -> String {
    let name = d.name().to_string_lossy().trim().to_string();
    if name.is_empty() {
        mount.trim_end_matches('\\').to_string()
    } else {
        name
    }
}

/// Core classes and clusters. Linux: `cpu_capacity` (big.LITTLE: A55 = 414, A76 = 1024) or, failing
/// that, the maximum frequency, ranked so the fastest kind gets the highest class; clusters from
/// `physical_package_id`, which on ARM SoCs is the cluster. macOS: Apple silicon reports its
/// efficiency and performance core counts through sysctl, efficiency cores come first.
/// Link speed in bits/s: Linux reports Mbit/s in sysfs (-1 when the link is down).
fn link_speed(iface: &str) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let v: i64 = std::fs::read_to_string(format!("/sys/class/net/{iface}/speed")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        if v > 0 {
            return v as u64 * 1_000_000;
        }
    }
    let _ = iface;
    0
}

fn topology(n: usize) -> (Vec<u8>, Vec<u8>, usize, Vec<bool>) {
    #[cfg(target_os = "linux")]
    {
        let read = |i: usize, f: &str| -> Option<u64> { std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{i}/{f}")).ok()?.trim().parse().ok() };
        let caps: Vec<u64> = (0..n).map(|i| read(i, "cpu_capacity").or_else(|| read(i, "cpufreq/cpuinfo_max_freq")).unwrap_or(0)).collect();
        let mut kinds: Vec<u64> = caps.clone();
        kinds.sort_unstable();
        kinds.dedup();
        let classes: Vec<u8> = if kinds.len() > 1 { caps.iter().map(|c| kinds.iter().position(|k| k == c).unwrap_or(0) as u8).collect() } else { vec![0; n] };
        let pkgs: Vec<u64> = (0..n).map(|i| read(i, "topology/physical_package_id").unwrap_or(0)).collect();
        let mut ids: Vec<u64> = pkgs.clone();
        ids.sort_unstable();
        ids.dedup();
        let clusters: Vec<u8> = pkgs.iter().map(|p| ids.iter().position(|k| k == p).unwrap_or(0) as u8).collect();
        // on x86 the package id is a socket; on ARM it is a cluster of one SoC
        let sockets = if cfg!(target_arch = "x86_64") || cfg!(target_arch = "x86") { ids.len().max(1) } else { 1 };
        // SMT siblings: a later cpu with the same (package, core_id) as an earlier one
        let core_ids: Vec<(u64, u64)> = (0..n).map(|i| (pkgs[i], read(i, "topology/core_id").unwrap_or(i as u64))).collect();
        let smt: Vec<bool> = (0..n).map(|i| (0..i).any(|j| core_ids[j] == core_ids[i])).collect();
        (classes, clusters, sockets, smt)
    }
    #[cfg(target_os = "macos")]
    {
        let sysctl = |k: &str| -> Option<usize> {
            let out = std::process::Command::new("sysctl").args(["-n", k]).output().ok()?;
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        };
        // perflevel0 = performance cores, perflevel1 = efficiency cores (Apple silicon only)
        if let (Some(p), Some(e)) = (sysctl("hw.perflevel0.logicalcpu"), sysctl("hw.perflevel1.logicalcpu")) {
            if p + e == n && e > 0 {
                let mut classes = vec![0u8; e];
                classes.extend(std::iter::repeat(1u8).take(p));
                let mut clusters = vec![0u8; e];
                clusters.extend(std::iter::repeat(1u8).take(p));
                return (classes, clusters, 1, vec![false; n]);
            }
        }
        // Intel Macs with Hyper-Threading enumerate the two threads of a core next to each other
        let smt = match (sysctl("hw.physicalcpu"), sysctl("hw.logicalcpu")) {
            (Some(p), Some(l)) if l == 2 * p && l == n => (0..n).map(|i| i % 2 == 1).collect(),
            _ => vec![false; n],
        };
        (vec![0; n], vec![0; n], 1, smt)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (vec![0; n], vec![0; n], 1, vec![false; n])
    }
}

/// Current clock per core in MHz. Linux: cpufreq; macOS (Intel): the nominal frequency from
/// sysctl for every core; Windows: what sysinfo reports (the base clock).
fn core_freqs(sys: &System, n: usize) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let read = |i: usize| -> Option<u32> { std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")).ok()?.trim().parse::<u64>().ok().map(|khz| (khz / 1000) as u32) };
        let v: Vec<u32> = (0..n).map(|i| read(i).unwrap_or(0)).collect();
        if v.iter().any(|f| *f > 0) {
            return v;
        }
    }
    let v: Vec<u32> = sys.cpus().iter().map(|c| c.frequency() as u32).collect();
    // Apple silicon comes back as a few MHz from sysinfo, which is no reading
    if v.len() == n && v.iter().any(|f| *f >= 100) {
        v
    } else {
        Vec::new()
    }
}

fn os_name() -> String {
    if cfg!(target_os = "linux") {
        "linux".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "freebsd") {
        "freebsd".into()
    } else {
        std::env::consts::OS.into()
    }
}

/// Hardware interfaces with their kind, MAC, link speed and adapter.
fn list_ifaces(nets: &Networks, root: &Root) -> Vec<IfaceInfo> {
    #[cfg(target_os = "macos")]
    let kinds = crate::macos::net::kinds();
    let mut ifaces = Vec::new();
    for (iname, data) in nets.iter() {
        if skip_iface(iname) {
            continue;
        }
        let mac = data.mac_address().to_string();
        // no MAC or no IPv4: bridge members, unplugged ports, virtual leftovers
        if mac == "00:00:00:00:00:00" || !data.ip_networks().iter().any(|n| n.addr.is_ipv4()) {
            continue;
        }
        #[allow(unused_mut)]
        let mut kind = if crate::linux::net::is_wifi(root, iname) { "Wi-Fi" } else { iface_kind(iname) };
        #[allow(unused_mut)]
        let mut description = crate::linux::net::description(root, iname);
        #[cfg(target_os = "macos")]
        if let Some((_, k, d)) = kinds.iter().find(|(n, ..)| n == iname) {
            kind = k;
            description = d.clone();
        }
        ifaces.push(IfaceInfo { id: iname.clone(), kind: kind.into(), mac, speed: link_speed(iname), description });
    }
    ifaces.sort_by(|a, b| a.id.cmp(&b.id));
    ifaces
}

/// A listed disk and where its numbers come from: the mounted block device (diskstats) and
/// the whole disk under it (sysfs).
#[derive(Clone, PartialEq)]
struct DiskSrc {
    id: String,
    stats: String,
    whole: String,
}

/// Mounted storage, one entry per device, with what the hardware tells about it.
fn list_disks(disks: &Disks, root: &Root, #[allow(unused_variables)] internal: Option<&HwDisk>) -> (Vec<DiskInfo>, Vec<DiskSrc>) {
    let mut out: Vec<DiskInfo> = Vec::new();
    let mut src = Vec::new();
    for d in disks.list() {
        let fs = d.file_system().to_string_lossy().to_string();
        let mount = d.mount_point().to_string_lossy().to_string();
        let dname = disk_id(d, &mount);
        // a helper mount of nothing (VeraCrypt's FUSE control file system)
        if skip_mount(&fs, &dname, &mount) || out.iter().any(|x| x.id == dname) || d.total_space() == 0 {
            continue;
        }
        let kind = match d.kind() {
            sysinfo::DiskKind::SSD => Some(true),
            sysinfo::DiskKind::HDD => Some(false),
            _ => None,
        };
        let mut info = DiskInfo { id: dname.clone(), model: if dname == mount { mount.clone() } else { format!("{dname} ({mount})") }, bus: if d.is_removable() { "USB".into() } else { String::new() }, size: d.total_space(), ssd: kind, ..Default::default() };
        if let Some((stats, whole)) = crate::linux::block::resolve(root, &dname, &mount) {
            let hw = crate::linux::block::hw(root, &whole);
            apply_hw(&mut info, &mount, &hw.model, hw.bus, &hw.serial, &hw.firmware, hw.ssd);
            info.trim = hw.trim;
            src.push(DiskSrc { id: dname.clone(), stats, whole });
        }
        #[cfg(target_os = "macos")]
        if let Some(hw) = internal.filter(|_| mount == "/") {
            apply_hw(&mut info, &mount, &hw.model, hw.bus, &hw.serial, &hw.firmware, hw.ssd);
        }
        if info.bus == "Virtual" {
            // a virtual disk is neither
            info.ssd = None;
        }
        if info.bus.is_empty() {
            // what sysinfo knows when sysfs / IOKit said nothing
            info.bus = match kind {
                Some(true) => "SSD",
                Some(false) => "HDD",
                None => "",
            }
            .into();
        }
        out.push(info);
    }
    (out, src)
}

#[cfg(target_os = "macos")]
type HwDisk = crate::macos::disk::Hw;
#[cfg(not(target_os = "macos"))]
type HwDisk = ();

fn apply_hw(info: &mut DiskInfo, mount: &str, model: &str, bus: &str, serial: &str, firmware: &str, ssd: Option<bool>) {
    if !model.is_empty() {
        info.model = format!("{model} ({mount})");
    }
    if !bus.is_empty() && info.bus != "USB" {
        info.bus = bus.into();
    }
    info.serial = serial.into();
    info.firmware = firmware.into();
    info.ssd = ssd.or(info.ssd);
}

impl Collector {
    pub fn new(id: &str, name: &str, agent_version: &str) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let nets = Networks::new_with_refreshed_list();
        let disks = Disks::new_with_refreshed_list();
        let comps = Components::new_with_refreshed_list();

        let cores = sys.cpus().len();
        // "Cortex-A55 + Cortex-A76" on big.LITTLE parts, one name elsewhere
        let mut cpu_name = String::new();
        for c in sys.cpus() {
            let b = c.brand().trim();
            if !b.is_empty() && !cpu_name.split(" + ").any(|x| x == b) {
                if !cpu_name.is_empty() {
                    cpu_name.push_str(" + ");
                }
                cpu_name.push_str(b);
            }
        }
        let root = Root::system();
        if cpu_name.is_empty() {
            cpu_name = crate::linux::cpu::name(&root);
        }
        let name = if name.is_empty() { System::host_name().unwrap_or_else(|| "agent".into()) } else { name.to_string() };
        #[cfg(target_os = "macos")]
        let (os_version, kernel) = crate::macos::sys::os();
        #[cfg(not(target_os = "macos"))]
        let (os_version, kernel) = (
            crate::linux::sys::os_pretty(&root).unwrap_or_else(|| match (System::name(), System::os_version()) {
                (Some(n), Some(v)) => format!("{n} {v}"),
                (Some(n), None) => n,
                _ => String::new(),
            }),
            System::kernel_version().map(|k| if cfg!(windows) { format!("NT {k}") } else { k }).unwrap_or_default(),
        );
        let zones = crate::linux::thermal::zones(&root);
        let gpu = crate::linux::gpu::Gpu::find(&root, &zones);
        let zones = crate::linux::thermal::shown(&root, &zones);
        let fans = crate::linux::hwmon::Fans::find(&root);
        let rails = crate::linux::hwmon::Rails::find(&root);
        let (classes, clusters, sockets, smt) = topology(cores);
        #[cfg(target_os = "macos")]
        let mac = crate::macos::Mac::new(&classes);
        #[cfg(target_os = "macos")]
        let (gpu_name, gpus, battery, internal) = (mac.gpu_name(), mac.gpus(), mac.has_battery(), mac.disk_hw());
        #[cfg(not(target_os = "macos"))]
        let (gpu_name, gpus, battery, internal) = (gpu.as_ref().map(|g| g.name.clone()), gpu.as_ref().map(|g| g.all.clone()).unwrap_or_default(), crate::linux::power::read_battery(&root).is_some(), None::<HwDisk>);
        let ifaces = list_ifaces(&nets, &root);
        let (disk_infos, disk_src) = list_disks(&disks, &root, internal.as_ref());
        let mut has = vec!["cpu".to_string(), "mem".into(), "net".into(), "disk".into()];
        if gpu_name.is_some() {
            has.push("gpu".into());
        }
        if battery {
            has.push("battery".into());
        }
        if !comps.list().is_empty() || !zones.is_empty() {
            has.push("temp".into());
        }
        let (features, all_features) = crate::features::read();
        // caches, clocks, virtualization and memory modules: sysfs / SMBIOS on Linux, sysctl and
        // System Information on a Mac, CPUID where the rest says nothing
        let (cpuid_base, cpuid_max) = crate::features::x86_clocks();
        #[cfg(target_os = "macos")]
        let ((virtualization, mut hypervisor), (mem_summary, mem_channels), cache, (base_mhz, max_mhz)) = (crate::macos::sys::virtualization(), crate::macos::sys::memory(), crate::macos::sys::caches(), (crate::macos::sys::base_mhz(), mac.cpu_max_mhz()));
        #[cfg(not(target_os = "macos"))]
        let ((virtualization, mut hypervisor), (mem_summary, mem_channels), cache, (base_mhz, max_mhz)) = (crate::linux::sys::virtualization(&root), crate::linux::dmi::memory(&root), crate::linux::sys::caches(&root), crate::linux::sys::clocks(&root));
        let base_mhz = base_mhz.or(cpuid_base);
        let max_mhz = cpuid_max.or(max_mhz).filter(|m| base_mhz.map_or(true, |b| *m >= b));
        hypervisor |= crate::features::x86_hypervisor();
        let info = Info {
            v: VERSION,
            id: id.to_string(),
            name,
            os: os_name(),
            os_version,
            kernel,
            arch: System::cpu_arch(),
            agent: agent_version.to_string(),
            cpu_name,
            cores,
            classes,
            clusters,
            smt,
            sockets,
            mem_total: sys.total_memory(),
            swap_total: sys.total_swap(),
            disks: disk_infos,
            ifaces,
            has,
            gpu_name: gpu_name.unwrap_or_default(),
            gpus,
            features,
            all_features,
            cache,
            base_mhz,
            max_mhz,
            virtualization,
            hypervisor,
            mem_summary,
            mem_channels,
        };
        Self {
            sys,
            nets,
            disks,
            comps,
            zones,
            gpu,
            fans,
            rails,
            #[cfg(target_os = "macos")]
            mac,
            last: Instant::now(),
            seq: 0,
            procs: None,
            procs_at: None,
            procs_prev_io: HashMap::new(),
            #[cfg(windows)]
            wincpu: crate::wincpu::WinCpu::new(cores),
            #[cfg(not(windows))]
            unixcpu: crate::unixcpu::UnixCpu::new(cores),
            last_cpu: 0.0,
            info,
            disk_src,
            diskstats: Default::default(),
            syscnt: crate::linux::sys::SysCounters::new(root.clone()),
            wifi: Default::default(),
            lists_at: Instant::now(),
            root,
        }
    }

    /// Interfaces and disks come and go (a USB stick, a dongle, a cable): the lists are taken
    /// again every 30 s and `info` changes when they did.
    fn refresh_lists(&mut self) {
        if self.lists_at.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.lists_at = Instant::now();
        #[cfg(target_os = "macos")]
        let internal = self.mac.disk_hw();
        #[cfg(not(target_os = "macos"))]
        let internal: Option<HwDisk> = None;
        let ifaces = list_ifaces(&self.nets, &self.root);
        let (disks, src) = list_disks(&self.disks, &self.root, internal.as_ref());
        if ifaces != self.info.ifaces || disks != self.info.disks {
            self.info.ifaces = ifaces;
            self.info.disks = disks;
            self.disk_src = src;
        }
    }

    /// One sample; call once per interval.
    pub fn sample(&mut self) -> Snapshot {
        self.refresh_lists();
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64().max(0.05);
        self.last = now;
        self.seq += 1;

        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh(true);
        self.disks.refresh(true);
        self.comps.refresh(true);

        #[allow(unused_mut)]
        let mut freq = core_freqs(&self.sys, self.info.cores);
        #[cfg(target_os = "macos")]
        let mac = self.mac.sample();
        #[cfg(target_os = "macos")]
        if mac.freq.len() == self.info.cores {
            freq = mac.freq.clone();
        }
        #[cfg(not(windows))]
        let cpu = {
            self.unixcpu.update();
            let u = &self.unixcpu;
            if u.works() && u.cores.len() == self.info.cores {
                CpuSample { total: u.total, sys: u.sys, user: u.user, cores: u.cores.clone(), freq }
            } else {
                let cores: Vec<f32> = self.sys.cpus().iter().map(|c| (c.cpu_usage() / 100.0).clamp(0.0, 1.0)).collect();
                let total = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f32>() / cores.len() as f32 };
                CpuSample { total, sys: 0.0, user: total, cores, freq }
            }
        };
        #[cfg(windows)]
        let cpu = {
            self.wincpu.update();
            CpuSample { total: self.wincpu.total, sys: self.wincpu.sys, user: self.wincpu.user, cores: self.wincpu.cores.clone(), freq }
        };
        self.last_cpu = cpu.total;

        let mem = MemSample { used: self.sys.used_memory(), avail: self.sys.available_memory(), swap_used: self.sys.used_swap() };

        let mut net = Vec::new();
        for (name, data) in self.nets.iter() {
            if !self.info.ifaces.iter().any(|i| i.id == *name) {
                continue;
            }
            net.push(NetSample { id: name.clone(), rx: data.received() as f64 / dt, tx: data.transmitted() as f64 / dt, up: true });
        }
        net.sort_by(|a, b| a.id.cmp(&b.id));

        self.diskstats.refresh(&self.root);
        let mut disk = Vec::new();
        for d in self.disks.list() {
            let dname = disk_id(d, &d.mount_point().to_string_lossy());
            if !self.info.disks.iter().any(|x| x.id == dname) || disk.iter().any(|x: &DiskSample| x.id == dname) {
                continue;
            }
            let u = d.usage();
            let mut s = DiskSample { id: dname, r: u.read_bytes as f64 / dt, w: u.written_bytes as f64 / dt, ..Default::default() };
            if let Some(src) = self.disk_src.iter().find(|x| x.id == s.id) {
                if let Some(a) = self.diskstats.activity(&src.stats) {
                    (s.active, s.queue, s.latency_ms, s.iops) = (Some(a.active), Some(a.queue), a.latency_ms, Some(a.iops));
                }
                s.temp = crate::linux::block::temp(&self.root, &src.whole);
            }
            disk.push(s);
        }

        let mut temp = Vec::new();
        for c in self.comps.list() {
            if let Some(t) = c.temperature() {
                if t.is_finite() && t > 0.0 {
                    temp.push(TempSample { id: c.label().to_string(), c: t });
                }
            }
        }
        // thermal zones: all of Jetson's sensors, x86_pkg_temp and acpitz next to hwmon
        crate::linux::thermal::merge(&mut temp, &self.zones);

        let la = System::load_average();
        let load = if la.one > 0.0 || la.five > 0.0 { vec![la.one as f32, la.five as f32, la.fifteen as f32] } else { Vec::new() };

        #[cfg(target_os = "macos")]
        if let (Some((iops, latency)), Some(d)) = (mac.disk, self.info.disks.iter().find(|d| d.model.ends_with("(/)")).and_then(|i| disk.iter_mut().find(|s| s.id == i.id))) {
            (d.iops, d.latency_ms) = (Some(iops), latency);
        }
        #[cfg(target_os = "macos")]
        let (gpu, battery, fans, power) = (mac.gpu, mac.battery, mac.fans, mac.power);
        #[cfg(not(target_os = "macos"))]
        let (gpu, battery, fans, power) = (self.gpu.as_mut().map(|g| g.sample()), crate::linux::power::read_battery(&self.root), self.fans.sample(), self.rails.sample());
        Snapshot { seq: self.seq, cpu, mem, net, disk, gpu, battery, temp, load, fans, power, battery_point: None }
    }

    /// Popup material: top processes and mounted volumes. The process table is refreshed at
    /// most once a second however many masters ask.
    pub fn details(&mut self, n: usize) -> Details {
        let n = n.clamp(1, 50);
        let now = Instant::now();
        let fresh = self.procs_at.map_or(false, |t| now.duration_since(t).as_millis() < 900);
        if !fresh {
            let procs = self.procs.get_or_insert_with(System::new);
            procs.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage());
            self.procs_at = Some(now);
        }
        let cores = self.info.cores.max(1) as f32;
        let mut rows: Vec<ProcSample> = Vec::new();
        if let Some(procs) = &self.procs {
            let mut seen = HashMap::new();
            for (pid, p) in procs.processes() {
                // threads are listed as processes on Linux; the process itself is enough
                if p.thread_kind().is_some() {
                    continue;
                }
                let pid = pid.as_u32();
                let du = p.disk_usage();
                let io = match self.procs_prev_io.get(&pid) {
                    Some((r, w, t)) => {
                        let dt = now.duration_since(*t).as_secs_f64().max(0.05);
                        (du.total_read_bytes.saturating_sub(*r) + du.total_written_bytes.saturating_sub(*w)) as f64 / dt
                    }
                    None => 0.0,
                };
                seen.insert(pid, (du.total_read_bytes, du.total_written_bytes, now));
                rows.push(ProcSample {
                    pid,
                    name: p.name().to_string_lossy().to_string(),
                    cpu: (p.cpu_usage() / 100.0 / cores).clamp(0.0, 1.0),
                    mem: p.memory(),
                    io,
                    ..Default::default()
                });
            }
            self.procs_prev_io = seen;
        }
        let mut top_cpu = rows.clone();
        top_cpu.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
        top_cpu.truncate(n);
        let mut top_mem = rows.clone();
        top_mem.sort_by(|a, b| b.mem.cmp(&a.mem));
        top_mem.truncate(n);
        // processes on the GPU (Linux DRM fdinfo and NVML), with their rows for name, CPU and memory
        #[cfg(not(target_os = "macos"))]
        let gpu_procs = self.gpu.as_ref().map(|g| g.procs()).unwrap_or_default();
        #[cfg(target_os = "macos")]
        let gpu_procs: Vec<(u32, f32, u64)> = Vec::new();
        let mut top_gpu: Vec<ProcSample> = gpu_procs
            .iter()
            .map(|(pid, load, vram)| {
                let base = rows.iter().find(|r| r.pid == *pid).cloned().unwrap_or_else(|| ProcSample { pid: *pid, name: format!("pid {pid}"), ..Default::default() });
                ProcSample { gpu: *load, vram: *vram, ..base }
            })
            .collect();
        top_gpu.sort_by(|a, b| b.gpu.partial_cmp(&a.gpu).unwrap_or(std::cmp::Ordering::Equal).then(b.vram.cmp(&a.vram)));
        top_gpu.truncate(n);
        let mut top_io: Vec<ProcSample> = rows.into_iter().filter(|p| p.io > 0.0).collect();
        top_io.sort_by(|a, b| b.io.partial_cmp(&a.io).unwrap_or(std::cmp::Ordering::Equal));
        top_io.truncate(n);

        let mut drives = Vec::new();
        for d in self.disks.list() {
            let fs = d.file_system().to_string_lossy().to_string();
            let mount = d.mount_point().to_string_lossy().to_string();
            let dname = disk_id(d, &mount);
            if skip_mount(&fs, &dname, &mount) {
                continue;
            }
            drives.push(DriveSample {
                id: mount,
                label: dname,
                fs,
                kind: if d.is_removable() { "USB".into() } else { String::new() },
                readonly: d.is_read_only(),
                total: d.total_space(),
                free: d.available_space(),
            });
        }

        let mut ips = Vec::new();
        for (name, data) in self.nets.iter() {
            if !self.info.ifaces.iter().any(|i| i.id == *name) {
                continue;
            }
            if let Some(v4) = data.ip_networks().iter().find(|n| n.addr.is_ipv4()) {
                ips.push((name.clone(), v4.addr.to_string()));
            }
        }

        // Wi-Fi network and link through `iw` (Linux)
        let (mut ssids, mut wifi) = (Vec::new(), Vec::new());
        for i in self.info.ifaces.iter().filter(|i| i.kind == "Wi-Fi") {
            if let Some((ssid, link)) = self.wifi.link(&i.id) {
                if !ssid.is_empty() {
                    ssids.push((i.id.clone(), ssid));
                }
                if !link.is_empty() {
                    wifi.push((i.id.clone(), link));
                }
            }
        }

        // processes, threads, open files, uptime, switches and interrupts; where memory goes
        #[cfg(target_os = "macos")]
        let mut sys = crate::macos::sys::counters();
        #[cfg(not(target_os = "macos"))]
        let mut sys = self.syscnt.sample();
        if sys.processes == 0 {
            sys.processes = self.procs.as_ref().map_or(0, |p| p.processes().values().filter(|p| p.thread_kind().is_none()).count() as u32);
        }
        sys.uptime_secs = System::uptime();
        #[cfg(target_os = "macos")]
        let mem = crate::macos::sys::mem_detail();
        #[cfg(not(target_os = "macos"))]
        let mem = crate::linux::sys::mem_detail(&self.root);
        let mem = mem.map(|mut m| {
            if m.swap_total == 0 {
                (m.swap_used, m.swap_total) = (self.sys.used_swap(), self.sys.total_swap());
            }
            m
        });

        Details { n, top_cpu, top_mem, top_io, top_gpu, drives, ips, ssids, sys: Some(sys), mem, wifi }
    }

    /// (address, prefix length) of every IPv4 on the listed interfaces, for per-interface beacons.
    pub fn local_ipv4s(&self) -> Vec<(std::net::Ipv4Addr, u8)> {
        let mut out = Vec::new();
        for (name, data) in self.nets.iter() {
            if !self.info.ifaces.iter().any(|i| i.id == *name) {
                continue;
            }
            for n in data.ip_networks() {
                if let std::net::IpAddr::V4(ip) = n.addr {
                    out.push((ip, n.prefix));
                }
            }
        }
        out
    }

    /// Current cpu / mem fractions for the beacon.
    pub fn quick(&self) -> (f32, f32) {
        let total = self.sys.total_memory().max(1);
        (self.last_cpu.clamp(0.0, 1.0), (self.sys.used_memory() as f32 / total as f32).clamp(0.0, 1.0))
    }
}
