//! "Share this machine": winstats acts as an agent itself, so other winstats on the network see
//! this Windows machine without a separate winstats-agent. Same beacon, same TCP protocol; the
//! data is the local `MetricsData` the strip already has.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use winstats_proto::*;

use crate::config::Config;
use crate::metrics::MetricsData;

struct Inner {
    info: Mutex<Info>,
    snap: Mutex<Snapshot>,
    changed: Condvar,
    details: Mutex<Details>,
    /// last time a master asked for details; the UI collects them while this is recent
    details_asked: Mutex<Option<Instant>>,
    token: String,
    port: u16,
    stop: AtomicBool,
    /// writers of the connected masters, for the goodbye when sharing stops
    clients: Mutex<Vec<Arc<Mutex<BufWriter<TcpStream>>>>>,
}

pub struct Share {
    inner: Arc<Inner>,
    seq: u64,
    /// cpu / mem for the beacon, set by publish
    quick: Arc<Mutex<(f32, f32)>>,
}

impl Drop for Share {
    fn drop(&mut self) {
        // sharing turned off, winstats closing or Windows shutting down: the masters drop this
        // machine's widgets at once
        for w in self.inner.clients.lock().unwrap().iter() {
            let _ = write_frame(&mut *w.lock().unwrap(), &Frame::Bye { reason: BYE_SHUTDOWN.into() });
        }
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.changed.notify_all();
    }
}

fn host_name(cfg: &Config) -> String {
    if !cfg.share_name.is_empty() {
        return cfg.share_name.clone();
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".into())
}

fn info_from(cfg: &Config, d: &MetricsData) -> Info {
    let mut has = vec!["cpu".to_string(), "mem".into(), "net".into(), "disk".into()];
    if d.gpu_available {
        has.push("gpu".into());
    }
    if d.battery_available {
        has.push("battery".into());
    }
    Info {
        v: VERSION,
        id: cfg.share_id.clone(),
        name: host_name(cfg),
        os: "windows".into(),
        os_version: format!("Windows {} (build {})", if crate::util::is_windows11() { "11" } else { "" }, crate::util::windows_build()).replace("  ", " "),
        kernel: format!("NT 10.0.{}", crate::util::windows_build()),
        arch: std::env::consts::ARCH.into(),
        agent: format!("winstats {}", crate::about::VERSION),
        cpu_name: d.cpu_name.clone(),
        cores: d.cpu_cores.len().max(d.cpu_classes.len()),
        classes: d.cpu_classes.clone(),
        clusters: d.cpu_packages.clone(),
        smt: d.cpu_smt.clone(),
        sockets: d.cpu_sockets.max(1),
        mem_total: d.mem.total,
        swap_total: d.mem.commit_total.saturating_sub(d.mem.total),
        disks: d
            .disks
            .iter()
            .map(|x| DiskInfo { id: format!("PhysicalDrive{}", x.index), model: x.model.clone(), bus: x.bus.to_string(), size: x.size, serial: x.serial.clone(), firmware: x.firmware.clone(), ssd: x.ssd, trim: x.trim })
            .collect(),
        ifaces: d.net_ifaces.iter().map(|i| IfaceInfo { id: i.alias.clone(), kind: i.kind.to_string(), mac: i.mac.clone(), speed: i.speed, description: i.description.clone() }).collect(),
        has,
        gpu_name: d.gpu_name.clone(),
        gpus: d.gpu_adapters.iter().filter(|a| a.enabled).map(|a| GpuInfo { name: a.name.clone(), vram_total: a.dedicated, vendor_id: a.vendor_id, device_id: a.device_id, driver: a.driver.clone() }).collect(),
        features: d.sys.cpu.features.clone(),
        all_features: d.sys.cpu.all_features.clone(),
        cache: d.sys.cpu.cache,
        base_mhz: d.sys.cpu.base_mhz,
        max_mhz: d.sys.cpu.max_mhz,
        virtualization: d.sys.cpu.virtualization,
        hypervisor: d.sys.cpu.hypervisor,
        mem_summary: d.mem_summary.clone().unwrap_or_default(),
        mem_channels: d.mem_channels.unwrap_or(0),
    }
}

fn snapshot_from(d: &MetricsData, seq: u64) -> Snapshot {
    Snapshot {
        seq,
        cpu: CpuSample { total: d.cpu_total, sys: d.cpu_system, user: d.cpu_user, cores: d.cpu_cores.clone(), freq: d.cpu_freq.clone() },
        mem: MemSample { used: d.mem.used, avail: d.mem.total.saturating_sub(d.mem.used), swap_used: d.mem.commit_used.saturating_sub(d.mem.used) },
        net: d.net_ifaces.iter().map(|i| NetSample { id: i.alias.clone(), rx: i.rx_bps, tx: i.tx_bps, up: i.up }).collect(),
        disk: d
            .disks
            .iter()
            .map(|x| DiskSample { id: format!("PhysicalDrive{}", x.index), r: x.read_bps, w: x.write_bps, active: x.active, queue: x.queue, latency_ms: x.latency_ms, iops: x.iops, temp: x.temp })
            .collect(),
        gpu: d.gpu.map(|g| GpuSample {
            load: g,
            // the adapter's own counter when read; DXGI's figure is winstats' own use
            vram_used: d.gpu_primary.as_ref().and_then(|p| d.gpu_adapters.iter().find(|a| a.luid == p.luid)).and_then(|a| a.dedicated_used).unwrap_or(d.vram_used),
            vram_total: d.vram_budget.max(d.vram_total),
            temp: d.gpu_perf.temp,
            hotspot: d.gpu_perf.hotspot,
            core_mhz: d.gpu_perf.core_mhz,
            core_max_mhz: d.gpu_perf.core_max_mhz,
            mem_mhz: d.gpu_perf.mem_mhz,
            mem_max_mhz: d.gpu_perf.mem_max_mhz,
            power_w: d.gpu_perf.power_w,
            power_limit_w: d.gpu_perf.power_limit_w,
            fan_rpm: d.gpu_perf.fan_rpm,
            fan_pct: d.gpu_perf.fan_pct,
            pcie_gen: d.gpu_perf.pcie_gen,
            pcie_width: d.gpu_perf.pcie_width,
            pcie_max_gen: d.gpu_perf.pcie_max_gen,
            pcie_max_width: d.gpu_perf.pcie_max_width,
        }),
        battery: d.battery.map(|b| {
            let det = d.battery_detail.unwrap_or_default();
            let wh = |mwh: u32| (mwh > 0).then(|| mwh as f32 / 1000.0);
            BatterySample {
                level: b.level,
                charging: b.charging,
                on_ac: b.on_ac,
                remaining_secs: b.remaining_secs,
                rate_w: d.battery_detail.map(|x| x.rate_mw as f32 / 1000.0),
                remaining_wh: wh(det.remaining_mwh),
                full_wh: wh(det.max_mwh),
                design_wh: wh(det.design_mwh),
                cycles: det.cycles,
            }
        }),
        temp: d.cpu_temp.map(|c| vec![TempSample { id: "CPU Package".into(), c }]).unwrap_or_default(),
        load: Vec::new(),
        fans: Vec::new(),
        power: Vec::new(),
    }
}

fn details_from(d: &MetricsData) -> Details {
    let conv = |v: &[crate::metrics::processes::ProcInfo]| -> Vec<ProcSample> { v.iter().map(|p| ProcSample { pid: p.pid, name: p.name.clone(), cpu: p.cpu, mem: p.mem, io: p.io, gpu: p.gpu, vram: p.vram }).collect() };
    Details {
        n: d.top_n,
        top_cpu: conv(&d.top_cpu),
        top_mem: conv(&d.top_mem),
        top_io: conv(&d.top_io),
        top_gpu: conv(&d.top_gpu),
        drives: d.drives.iter().map(|v| DriveSample { id: v.name.clone(), label: v.label.clone(), fs: v.fs.clone(), kind: v.kind.to_string(), readonly: v.readonly, total: v.total, free: v.free }).collect(),
        ips: d.net_ifaces.iter().filter(|i| !i.ip.is_empty()).map(|i| (i.alias.clone(), i.ip.clone())).collect(),
        ssids: d.net_ifaces.iter().filter(|i| !i.ssid.is_empty()).map(|i| (i.alias.clone(), i.ssid.clone())).collect(),
        sys: (d.sys.processes > 0).then(|| {
            let x = &d.sys;
            SysSample { processes: x.processes, threads: x.threads, handles: x.handles, uptime_secs: x.uptime_secs, ctx_switches: x.ctx_switches, interrupts: x.interrupts, dpc_time: x.dpc_time, interrupt_time: x.interrupt_time }
        }),
        mem: (d.mem_processes > 0 || d.sys.pagefile_total > 0).then(|| MemDetail {
            processes: d.mem_processes,
            kernel_paged: d.mem_kernel_paged,
            kernel_nonpaged: d.mem_kernel_nonpaged,
            cache: d.mem_cache,
            commit_used: d.mem.commit_used,
            commit_limit: d.mem.commit_total,
            swap_used: d.sys.pagefile_used,
            swap_total: d.sys.pagefile_total,
            hw_reserved: d.sys.hw_reserved,
        }),
        wifi: d.net_ifaces.iter().filter(|i| !i.wifi.is_empty()).map(|i| (i.alias.clone(), i.wifi.clone())).collect(),
    }
}

impl Share {
    pub fn start(cfg: &Config, d: &MetricsData) -> Option<Share> {
        let inner = Arc::new(Inner {
            info: Mutex::new(info_from(cfg, d)),
            snap: Mutex::new(Snapshot::default()),
            changed: Condvar::new(),
            details: Mutex::new(details_from(d)),
            details_asked: Mutex::new(None),
            token: cfg.share_token.clone(),
            port: cfg.share_port,
            stop: AtomicBool::new(false),
            clients: Mutex::new(Vec::new()),
        });
        let listener = match TcpListener::bind(("0.0.0.0", cfg.share_port)) {
            Ok(l) => l,
            Err(e) => {
                crate::log!("share: cannot listen on port {}: {e}", cfg.share_port);
                return None;
            }
        };
        let _ = listener.set_nonblocking(true);
        let quick = Arc::new(Mutex::new((0.0f32, 0.0f32)));
        {
            let inner = inner.clone();
            std::thread::Builder::new().name("share server".into()).spawn(move || serve(listener, inner)).ok()?;
        }
        {
            let inner = inner.clone();
            let quick = quick.clone();
            std::thread::Builder::new().name("share beacon".into()).spawn(move || beacon(inner, quick)).ok()?;
        }
        crate::log!("share: listening on port {}, beacons on", cfg.share_port);
        Some(Share { inner, seq: 0, quick })
    }

    /// Once per tick from the UI thread.
    pub fn publish(&mut self, cfg: &Config, d: &MetricsData) {
        self.seq += 1;
        *self.inner.snap.lock().unwrap() = snapshot_from(d, self.seq);
        *self.quick.lock().unwrap() = (d.cpu_total, d.mem.pct);
        if self.seq % 30 == 1 {
            // hardware lists can change (USB disks, adapters): refresh what new masters get
            *self.inner.info.lock().unwrap() = info_from(cfg, d);
        }
        if self.details_wanted() {
            *self.inner.details.lock().unwrap() = details_from(d);
        }
        self.inner.changed.notify_all();
    }

    /// A master asked for details within the last few seconds: keep collecting them.
    pub fn details_wanted(&self) -> bool {
        self.inner.details_asked.lock().unwrap().map_or(false, |t| t.elapsed().as_secs() < 4)
    }
}

fn beacon(inner: Arc<Inner>, quick: Arc<Mutex<(f32, f32)>>) {
    let mcast: SocketAddr = format!("{MULTICAST_ADDR}:{BEACON_PORT}").parse().unwrap();
    let bcast = SocketAddr::from((Ipv4Addr::BROADCAST, BEACON_PORT));
    let mut socks: HashMap<String, UdpSocket> = HashMap::new();
    let any = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = &any {
        let _ = s.set_multicast_ttl_v4(1);
        let _ = s.set_broadcast(true);
    }
    while !inner.stop.load(Ordering::Relaxed) {
        let (cpu, mem) = *quick.lock().unwrap();
        let b = {
            let i = inner.info.lock().unwrap();
            Beacon { v: VERSION, id: i.id.clone(), name: i.name.clone(), os: i.os.clone(), arch: i.arch.clone(), port: inner.port, agent: i.agent.clone(), cpu, mem }
        };
        if let Ok(body) = serde_json::to_vec(&b) {
            // one socket per local IPv4 so every segment hears the multicast
            for (_, ip) in crate::metrics::net::ipv4_table() {
                let s = socks.entry(ip.clone()).or_insert_with(|| {
                    let s = UdpSocket::bind((ip.as_str(), 0)).or_else(|_| UdpSocket::bind("0.0.0.0:0")).unwrap();
                    let _ = s.set_multicast_ttl_v4(1);
                    let _ = s.set_broadcast(true);
                    s
                });
                let _ = s.send_to(&body, mcast);
            }
            if let Some(s) = &any {
                let _ = s.send_to(&body, bcast);
            }
        }
        for _ in 0..(BEACON_INTERVAL_SECS * 5) {
            if inner.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn serve(listener: TcpListener, inner: Arc<Inner>) {
    while !inner.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, peer)) => {
                let inner = inner.clone();
                std::thread::Builder::new()
                    .name(format!("share {peer}"))
                    .spawn(move || {
                        if let Err(e) = client(s, inner) {
                            crate::log!("share {peer}: {e}");
                        }
                    })
                    .ok();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                crate::log!("share accept: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn client(stream: TcpStream, inner: Arc<Inner>) -> std::io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(PING_INTERVAL_SECS * 6)))?;
    // a master that stops reading must not hold up the goodbye
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let writer = Arc::new(Mutex::new(BufWriter::new(stream)));
    match read_frame(&mut reader)? {
        Frame::Hello { v, client, token } => {
            if v != VERSION {
                write_frame(&mut *writer.lock().unwrap(), &Frame::Bye { reason: format!("protocol {v}, this winstats speaks {VERSION}") })?;
                return Ok(());
            }
            if !inner.token.is_empty() && token != inner.token {
                write_frame(&mut *writer.lock().unwrap(), &Frame::Bye { reason: "bad token".into() })?;
                return Ok(());
            }
            crate::log!("share: {peer} connected ({client})");
        }
        _ => {
            write_frame(&mut *writer.lock().unwrap(), &Frame::Bye { reason: "hello expected".into() })?;
            return Ok(());
        }
    }
    let info = inner.info.lock().unwrap().clone();
    write_frame(&mut *writer.lock().unwrap(), &Frame::Info(info.clone()))?;
    inner.clients.lock().unwrap().push(writer.clone());

    let alive = Arc::new(AtomicBool::new(true));
    {
        let inner = inner.clone();
        let writer = writer.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut last_seq = 0u64;
            let mut sent = info;
            while alive.load(Ordering::Relaxed) && !inner.stop.load(Ordering::Relaxed) {
                let snap = {
                    let guard = inner.snap.lock().unwrap();
                    let (guard, _) = inner.changed.wait_timeout(guard, Duration::from_secs(2)).unwrap();
                    guard.clone()
                };
                if snap.seq == last_seq {
                    continue;
                }
                last_seq = snap.seq;
                // a disk or adapter came or went since the master connected
                let info = inner.info.lock().unwrap().clone();
                if info != sent {
                    if write_frame(&mut *writer.lock().unwrap(), &Frame::Info(info.clone())).is_err() {
                        break;
                    }
                    sent = info;
                }
                if write_frame(&mut *writer.lock().unwrap(), &Frame::Snapshot(snap)).is_err() {
                    break;
                }
            }
        });
    }
    let result = loop {
        if inner.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match read_frame(&mut reader) {
            Ok(Frame::Ping) => {
                if let Err(e) = write_frame(&mut *writer.lock().unwrap(), &Frame::Pong) {
                    break Err(e);
                }
            }
            Ok(Frame::Details(_)) => {
                *inner.details_asked.lock().unwrap() = Some(Instant::now());
                let det = inner.details.lock().unwrap().clone();
                if let Err(e) = write_frame(&mut *writer.lock().unwrap(), &Frame::Details(det)) {
                    break Err(e);
                }
            }
            Ok(Frame::Bye { .. }) => break Ok(()),
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    alive.store(false, Ordering::Relaxed);
    inner.changed.notify_all();
    inner.clients.lock().unwrap().retain(|w| !Arc::ptr_eq(w, &writer));
    crate::log!("share: {peer} disconnected");
    result
}

/// 32 hex digits, generated once and kept in the config.
pub fn new_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut out = String::new();
    for salt in 0u64..2 {
        let mut h = RandomState::new().build_hasher();
        salt.hash(&mut h);
        std::time::SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}
