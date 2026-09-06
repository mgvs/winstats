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
}

pub struct Share {
    inner: Arc<Inner>,
    seq: u64,
    /// cpu / mem for the beacon, set by publish
    quick: Arc<Mutex<(f32, f32)>>,
}

impl Drop for Share {
    fn drop(&mut self) {
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
        arch: std::env::consts::ARCH.into(),
        agent: format!("winstats {}", crate::about::VERSION),
        cpu_name: d.cpu_name.clone(),
        cores: d.cpu_cores.len().max(d.cpu_classes.len()),
        classes: d.cpu_classes.clone(),
        clusters: d.cpu_packages.clone(),
        sockets: d.cpu_sockets.max(1),
        mem_total: d.mem.total,
        swap_total: d.mem.commit_total.saturating_sub(d.mem.total),
        disks: d.disks.iter().map(|x| DiskInfo { id: format!("PhysicalDrive{}", x.index), model: x.model.clone(), bus: x.bus.to_string(), size: x.size }).collect(),
        ifaces: d.net_ifaces.iter().map(|i| IfaceInfo { id: i.alias.clone(), kind: i.kind.to_string(), mac: String::new() }).collect(),
        has,
    }
}

fn snapshot_from(d: &MetricsData, seq: u64) -> Snapshot {
    Snapshot {
        seq,
        cpu: CpuSample { total: d.cpu_total, sys: d.cpu_system, user: d.cpu_user, cores: d.cpu_cores.clone(), freq: d.cpu_freq.clone() },
        mem: MemSample { used: d.mem.used, avail: d.mem.total.saturating_sub(d.mem.used), swap_used: d.mem.commit_used.saturating_sub(d.mem.used) },
        net: d.net_ifaces.iter().map(|i| NetSample { id: i.alias.clone(), rx: i.rx_bps, tx: i.tx_bps, up: i.up }).collect(),
        disk: d.disks.iter().map(|x| DiskSample { id: format!("PhysicalDrive{}", x.index), r: x.read_bps, w: x.write_bps }).collect(),
        gpu: d.gpu.map(|g| GpuSample { load: g, vram_used: d.vram_used, vram_total: d.vram_budget.max(d.vram_total) }),
        battery: d.battery.map(|b| BatterySample { level: b.level, charging: b.charging, on_ac: b.on_ac, remaining_secs: b.remaining_secs }),
        temp: Vec::new(),
        load: Vec::new(),
    }
}

fn details_from(d: &MetricsData) -> Details {
    let conv = |v: &[crate::metrics::processes::ProcInfo]| -> Vec<ProcSample> { v.iter().map(|p| ProcSample { pid: p.pid, name: p.name.clone(), cpu: p.cpu, mem: p.mem, io: p.io }).collect() };
    Details {
        n: d.top_n,
        top_cpu: conv(&d.top_cpu),
        top_mem: conv(&d.top_mem),
        top_io: conv(&d.top_io),
        drives: d.drives.iter().map(|v| DriveSample { id: v.name.clone(), label: v.label.clone(), fs: v.fs.clone(), kind: v.kind.to_string(), readonly: v.readonly, total: v.total, free: v.free }).collect(),
        ips: d.net_ifaces.iter().filter(|i| !i.ip.is_empty()).map(|i| (i.alias.clone(), i.ip.clone())).collect(),
        ssids: d.net_ifaces.iter().filter(|i| !i.ssid.is_empty()).map(|i| (i.alias.clone(), i.ssid.clone())).collect(),
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
    write_frame(&mut *writer.lock().unwrap(), &Frame::Info(inner.info.lock().unwrap().clone()))?;

    let alive = Arc::new(AtomicBool::new(true));
    {
        let inner = inner.clone();
        let writer = writer.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut last_seq = 0u64;
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
