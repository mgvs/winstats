//! Metrics through `sysinfo`: one refresh per interval into a `Snapshot`, and heavier
//! per-process / per-mount material on demand for `Details`.

use std::collections::HashMap;
use std::time::Instant;
use sysinfo::{Components, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};
use winstats_proto::*;

pub struct Collector {
    sys: System,
    nets: Networks,
    disks: Disks,
    comps: Components,
    last: Instant,
    seq: u64,
    /// process table: refreshed only when a master asks for details
    procs: Option<System>,
    procs_at: Option<Instant>,
    procs_prev_io: HashMap<u32, (u64, u64, Instant)>,
    #[cfg(windows)]
    wincpu: crate::wincpu::WinCpu,
    /// last cpu fraction, for the beacon
    last_cpu: f32,
    pub info: Info,
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
fn topology(n: usize) -> (Vec<u8>, Vec<u8>, usize) {
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
        (classes, clusters, sockets)
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
                return (classes, clusters, 1);
            }
        }
        (vec![0; n], vec![0; n], 1)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (vec![0; n], vec![0; n], 1)
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
    if v.len() == n && v.iter().any(|f| *f > 0) {
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
        let name = if name.is_empty() { System::host_name().unwrap_or_else(|| "agent".into()) } else { name.to_string() };
        let os_version = match (System::name(), System::os_version()) {
            (Some(n), Some(v)) => format!("{n} {v}"),
            (Some(n), None) => n,
            _ => String::new(),
        };
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
            ifaces.push(IfaceInfo { id: iname.clone(), kind: iface_kind(iname).into(), mac });
        }
        ifaces.sort_by(|a, b| a.id.cmp(&b.id));
        let mut disk_infos: Vec<DiskInfo> = Vec::new();
        for d in disks.list() {
            let fs = d.file_system().to_string_lossy().to_string();
            let mount = d.mount_point().to_string_lossy().to_string();
            let dname = disk_id(d, &mount);
            if skip_mount(&fs, &dname, &mount) || disk_infos.iter().any(|x| x.id == dname) {
                continue;
            }
            let bus = if d.is_removable() { "USB" } else { match d.kind() { sysinfo::DiskKind::SSD => "SSD", sysinfo::DiskKind::HDD => "HDD", _ => "" } };
            let model = if dname == mount { mount.clone() } else { format!("{dname} ({mount})") };
            disk_infos.push(DiskInfo { id: dname.clone(), model, bus: bus.into(), size: d.total_space() });
        }
        let mut has = vec!["cpu".to_string(), "mem".into(), "net".into(), "disk".into()];
        if !comps.list().is_empty() {
            has.push("temp".into());
        }
        let (classes, clusters, sockets) = topology(cores);
        let info = Info {
            v: VERSION,
            id: id.to_string(),
            name,
            os: os_name(),
            os_version,
            arch: System::cpu_arch(),
            agent: agent_version.to_string(),
            cpu_name,
            cores,
            classes,
            clusters,
            sockets,
            mem_total: sys.total_memory(),
            swap_total: sys.total_swap(),
            disks: disk_infos,
            ifaces,
            has,
        };
        Self {
            sys,
            nets,
            disks,
            comps,
            last: Instant::now(),
            seq: 0,
            procs: None,
            procs_at: None,
            procs_prev_io: HashMap::new(),
            #[cfg(windows)]
            wincpu: crate::wincpu::WinCpu::new(cores),
            last_cpu: 0.0,
            info,
        }
    }

    /// One sample; call once per interval.
    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64().max(0.05);
        self.last = now;
        self.seq += 1;

        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh(true);
        self.disks.refresh(true);
        self.comps.refresh(true);

        let freq = core_freqs(&self.sys, self.info.cores);
        #[cfg(not(windows))]
        let cpu = {
            let cores: Vec<f32> = self.sys.cpus().iter().map(|c| (c.cpu_usage() / 100.0).clamp(0.0, 1.0)).collect();
            let total = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f32>() / cores.len() as f32 };
            CpuSample { total, sys: 0.0, user: total, cores, freq }
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

        let mut disk = Vec::new();
        for d in self.disks.list() {
            let dname = disk_id(d, &d.mount_point().to_string_lossy());
            if !self.info.disks.iter().any(|x| x.id == dname) || disk.iter().any(|x: &DiskSample| x.id == dname) {
                continue;
            }
            let u = d.usage();
            disk.push(DiskSample { id: dname, r: u.read_bytes as f64 / dt, w: u.written_bytes as f64 / dt });
        }

        let mut temp = Vec::new();
        for c in self.comps.list() {
            if let Some(t) = c.temperature() {
                if t.is_finite() && t > 0.0 {
                    temp.push(TempSample { id: c.label().to_string(), c: t });
                }
            }
        }

        let la = System::load_average();
        let load = if la.one > 0.0 || la.five > 0.0 { vec![la.one as f32, la.five as f32, la.fifteen as f32] } else { Vec::new() };

        Snapshot { seq: self.seq, cpu, mem, net, disk, gpu: None, battery: None, temp, load }
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

        Details { n, top_cpu, top_mem, top_io, drives, ips, ssids: Vec::new() }
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
