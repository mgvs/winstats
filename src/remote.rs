//! Remote machines: discovery of agents on the LAN (UDP beacons), one link per configured
//! agent (TCP, reconnecting), and the translation of their frames into `MetricsData` so the
//! widgets and popups draw them like the local machine.
//!
//! Threads never touch the UI: the discovery thread fills a table, every link thread queues
//! frames; the UI thread drains both in `Remotes::poll` once per tick.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winstats_proto::*;

use crate::config::{Config, RemoteCfg};
use crate::metrics::{self, battery::{Battery, BatteryDetail}, gpu::AdapterInfo, disks::PhysicalDisk, drives::Drive, net::NetIface, processes::ProcInfo, Fan, MetricsData, OsInfo, Power};

/// No data for this long: the machine's widgets turn grey.
pub const STALE_SECS: f64 = 10.0;
/// No data for this long: the machine's widgets leave the strip (they come back with the data).
/// Long enough to ride out a Wi-Fi roam or a router hiccup and the reconnect after it.
pub const GONE_SECS: f64 = 120.0;

/// An agent heard on the network.
#[derive(Clone, Debug)]
pub struct Seen {
    pub beacon: Beacon,
    /// where the last beacon came from
    pub addr: SocketAddr,
    pub at: Instant,
    /// every address the agent beacons from (one per interface on the network), last heard
    heard: Vec<(SocketAddr, Instant)>,
}

impl Seen {
    /// The agent was heard from this address within `BEACON_TIMEOUT_SECS`.
    fn heard_from(&self, addr: &str) -> bool {
        self.heard.iter().any(|(a, t)| t.elapsed().as_secs() < BEACON_TIMEOUT_SECS && a.to_string() == addr)
    }

    /// The most recently heard address other than `addr`.
    fn other_than(&self, addr: &str) -> Option<SocketAddr> {
        self.heard.iter().filter(|(a, t)| t.elapsed().as_secs() < BEACON_TIMEOUT_SECS && a.to_string() != addr).max_by_key(|(_, t)| *t).map(|(a, _)| *a)
    }
}

/// Beacon listener.
pub struct Discovery {
    seen: Arc<Mutex<HashMap<String, Seen>>>,
    stop: Arc<AtomicBool>,
}

impl Discovery {
    fn start() -> Option<Discovery> {
        let ifaces: Vec<std::net::Ipv4Addr> = crate::metrics::net::ipv4_table().values().filter_map(|ip| ip.parse().ok()).collect();
        let sock = match open_beacon_socket(&ifaces) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("discovery: cannot open beacon socket: {e}");
                return None;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        let seen: Arc<Mutex<HashMap<String, Seen>>> = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let seen = seen.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("discovery".into())
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    while !stop.load(Ordering::Relaxed) {
                        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                        let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) else { continue };
                        if b.v != VERSION || b.id.is_empty() {
                            continue;
                        }
                        let addr = SocketAddr::new(from.ip(), b.port);
                        let now = Instant::now();
                        let mut seen = seen.lock().unwrap();
                        let mut heard = seen.remove(&b.id).map(|s| s.heard).unwrap_or_default();
                        heard.retain(|(a, t)| *a != addr && t.elapsed().as_secs() < BEACON_TIMEOUT_SECS);
                        heard.push((addr, now));
                        seen.insert(b.id.clone(), Seen { beacon: b, addr, at: now, heard });
                    }
                })
                .ok()?;
        }
        crate::log!("discovery: listening on {MULTICAST_ADDR}:{BEACON_PORT}");
        Some(Discovery { seen, stop })
    }

    /// Agents heard within the last `BEACON_TIMEOUT_SECS`.
    pub fn seen(&self) -> Vec<Seen> {
        let mut v: Vec<Seen> = self.seen.lock().unwrap().values().filter(|s| s.at.elapsed().as_secs() < BEACON_TIMEOUT_SECS).cloned().collect();
        v.sort_by(|a, b| a.beacon.name.cmp(&b.beacon.name));
        v
    }

    fn by_id(&self, id: &str) -> Option<Seen> {
        self.seen.lock().unwrap().get(id).filter(|s| s.at.elapsed().as_secs() < BEACON_TIMEOUT_SECS).cloned()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Connecting,
    Online,
    Offline,
}

struct LinkState {
    status: Status,
    frames: VecDeque<Frame>,
    /// number of process rows the UI wants; taken by the writer thread
    want_details: Option<usize>,
    last_rx: Option<Instant>,
    /// the agent said it is shutting down: its widgets leave the strip until data comes again
    gone: bool,
}

/// One TCP session with an agent, reconnecting with backoff until dropped.
pub struct Link {
    state: Arc<Mutex<LinkState>>,
    stop: Arc<AtomicBool>,
    pub addr: String,
    /// the token it says hello with: a new one in the config starts a new link
    token: String,
}

impl Link {
    fn start(addr: String, token: String) -> Link {
        let state = Arc::new(Mutex::new(LinkState { status: Status::Connecting, frames: VecDeque::new(), want_details: None, last_rx: None, gone: false }));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let state = state.clone();
            let stop = stop.clone();
            let addr = addr.clone();
            let token = token.clone();
            std::thread::Builder::new().name(format!("link {addr}")).spawn(move || link_thread(addr, token, state, stop)).ok();
        }
        Link { state, stop, addr, token }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn link_thread(addr: String, token: String, state: Arc<Mutex<LinkState>>, stop: Arc<AtomicBool>) {
    let mut backoff = 1u64;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let _ = session(&addr, &token, &state, &stop);
        let mut s = state.lock().unwrap();
        s.status = Status::Offline;
        // a session that received data was a working link: the next drop retries quickly again
        if s.last_rx.map_or(false, |t| t >= started) {
            backoff = 1;
        }
        drop(s);
        // wait out the backoff in small steps so a drop stops the thread quickly
        let until = Instant::now() + Duration::from_secs(backoff);
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(30);
    }
}

fn session(addr: &str, token: &str, state: &Arc<Mutex<LinkState>>, stop: &Arc<AtomicBool>) -> std::io::Result<()> {
    state.lock().unwrap().status = Status::Connecting;
    let sa = addr.to_socket_addrs()?.next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unresolvable"))?;
    let stream = TcpStream::connect_timeout(&sa, Duration::from_secs(4))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(PING_INTERVAL_SECS * 4)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream.try_clone()?);
    write_frame(&mut writer, &Frame::Hello { v: VERSION, client: format!("winstats {}", crate::about::VERSION), token: token.to_string() })?;

    // writer: pings, and details requests when the UI asks
    let done = Arc::new(AtomicBool::new(false));
    let writer_thread = {
        let state = state.clone();
        let done = done.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut last_ping = Instant::now();
            while !done.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                let want = state.lock().unwrap().want_details.take();
                if let Some(n) = want {
                    if write_frame(&mut writer, &Frame::Details(Details { n, ..Default::default() })).is_err() {
                        break;
                    }
                }
                if last_ping.elapsed() >= Duration::from_secs(PING_INTERVAL_SECS) {
                    last_ping = Instant::now();
                    if write_frame(&mut writer, &Frame::Ping).is_err() {
                        break;
                    }
                }
            }
            let _ = write_frame(&mut writer, &Frame::Bye { reason: "closing".into() });
        })
    };

    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match read_frame(&mut reader) {
            Ok(Frame::Bye { reason }) => {
                if reason == BYE_SHUTDOWN {
                    state.lock().unwrap().gone = true;
                }
                break Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, reason));
            }
            Ok(Frame::Pong) => {
                state.lock().unwrap().last_rx = Some(Instant::now());
            }
            Ok(frame) => {
                let mut s = state.lock().unwrap();
                s.status = Status::Online;
                s.last_rx = Some(Instant::now());
                s.gone = false;
                if s.frames.len() > 200 {
                    s.frames.pop_front();
                }
                s.frames.push_back(frame);
            }
            Err(e) => break Err(e),
        }
    };
    done.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = writer_thread.join();
    result
}

/// A configured agent on the master side.
pub struct Agent {
    pub key: String,
    pub link: Option<Link>,
    pub info: Option<Info>,
    pub data: MetricsData,
    pub online: bool,
    /// when the address last changed (see `Remotes::poll`)
    addr_since: Instant,
    /// still drawn: data arrived within `GONE_SECS`
    pub visible: bool,
    /// drawn in grey: no data for `STALE_SECS`, not yet gone
    pub stale: bool,
    /// unix seconds of the last battery history point
    hist_at: u64,
    /// when the link went down, for the popup
    pub offline_since: Option<Instant>,
    net_disabled: Vec<String>,
    disks_disabled: Vec<String>,
    last_sample: Option<Instant>,
    /// P-cores first, then E-cores (Group P / E cores in the menu)
    group_pe: bool,
    /// the agent reports the commit charge (Linux); otherwise RAM + swap stands in
    commit_known: bool,
    /// hardware order of the cores as the agent reports them
    hw_classes: Vec<u8>,
    hw_clusters: Vec<u8>,
    hw_smt: Vec<bool>,
    /// display index -> hardware index
    order: Vec<usize>,
}

impl Agent {
    fn new(key: &str, cfg: &RemoteCfg) -> Agent {
        let mut data = MetricsData::default();
        data.host = if cfg.name.is_empty() { key.to_string() } else { cfg.name.clone() };
        data.top_n = 5;
        Agent {
            key: key.to_string(),
            link: None,
            info: None,
            data,
            online: false,
            visible: false,
            stale: false,
            offline_since: None,
            addr_since: Instant::now(),
            hist_at: 0,
            net_disabled: cfg.net_disabled.clone(),
            disks_disabled: cfg.disks_disabled.clone(),
            last_sample: None,
            group_pe: cfg.group_pe,
            commit_known: false,
            hw_classes: Vec::new(),
            hw_clusters: Vec::new(),
            hw_smt: Vec::new(),
            order: Vec::new(),
        }
    }

    fn apply_info(&mut self, info: Info) {
        let d = &mut self.data;
        d.host = info.name.clone();
        d.cpu_name = info.cpu_name.clone();
        d.sys.cpu.features = info.features.clone();
        d.sys.cpu.all_features = info.all_features.clone();
        d.sys.cpu.cache = info.cache;
        d.sys.cpu.base_mhz = info.base_mhz;
        d.sys.cpu.max_mhz = info.max_mhz;
        d.sys.cpu.virtualization = info.virtualization;
        d.sys.cpu.hypervisor = info.hypervisor;
        // an agent names its version alone ("0.3.0"), winstats sharing itself "winstats 0.2.9"
        let agent = if info.agent.starts_with(|c: char| c.is_ascii_digit()) { format!("winstats-agent {}", info.agent) } else { info.agent.clone() };
        d.os = OsInfo { release: info.os_version.clone(), kernel: info.kernel.clone(), arch: info.arch.clone(), agent };
        d.mem_summary = (!info.mem_summary.is_empty()).then(|| info.mem_summary.clone());
        d.mem_channels = (info.mem_channels > 0).then_some(info.mem_channels);
        self.hw_classes = if info.classes.len() == info.cores { info.classes.clone() } else { vec![0; info.cores] };
        // clusters give the dividers in the core chart; the socket count is separate
        self.hw_clusters = if info.clusters.len() == info.cores { info.clusters.clone() } else { vec![0; info.cores] };
        self.hw_smt = if info.smt.len() == info.cores { info.smt.clone() } else { vec![false; info.cores] };
        let d = &mut self.data;
        d.cpu_sockets = info.sockets.max(1);
        // Info comes again when a disk or interface changes: the live values stay
        if d.cpu_cores.len() != info.cores {
            d.cpu_cores = vec![0.0; info.cores];
        }
        self.rebuild_order();
        let d = &mut self.data;
        d.mem.total = info.mem_total;
        d.mem.commit_total = info.mem_total + info.swap_total;
        d.gpu_available = info.has.iter().any(|h| h == "gpu");
        d.gpu_name = info.gpu_name.clone();
        // several GPUs are listed in the popup title, the reported one bright; one is the title
        d.gpu_adapters = if info.gpus.len() > 1 {
            info.gpus
                .iter()
                .enumerate()
                .map(|(i, g)| adapter(i, g))
                .collect()
        } else {
            Vec::new()
        };
        // the reported GPU's ids and driver for "More", also when it is the only one
        d.gpu_primary = d.gpu_adapters.iter().find(|a| a.name == info.gpu_name).cloned().or_else(|| info.gpus.iter().enumerate().find(|(_, g)| g.name == info.gpu_name).map(|(i, g)| adapter(i, g)));
        d.battery_available = info.has.iter().any(|h| h == "battery");
        let old_disks = std::mem::take(&mut d.disks);
        let old_ifaces = std::mem::take(&mut d.net_ifaces);
        d.disks = info
            .disks
            .iter()
            .enumerate()
            .map(|(i, x)| PhysicalDisk {
                index: i as u32,
                model: x.model.clone(),
                bus: bus_static(&x.bus),
                size: x.size,
                serial: x.serial.clone(),
                firmware: x.firmware.clone(),
                ssd: x.ssd,
                trim: x.trim,
                read_bps: 0.0,
                write_bps: 0.0,
                enabled: !self.disks_disabled.contains(&x.model),
                ..Default::default()
            })
            .collect();
        d.net_ifaces = info
            .ifaces
            .iter()
            .map(|x| NetIface {
                luid: 0,
                guid: Default::default(),
                alias: x.id.clone(),
                description: x.description.clone(),
                mac: x.mac.clone(),
                speed: x.speed,
                kind: kind_static(&x.kind),
                up: true,
                rx_bps: 0.0,
                tx_bps: 0.0,
                ip: String::new(),
                ssid: String::new(),
                wifi: String::new(),
                enabled: !self.net_disabled.contains(&x.id),
            })
            .collect();
        for x in &mut d.disks {
            if let Some(o) = old_disks.iter().find(|o| o.model == x.model) {
                (x.read_bps, x.write_bps, x.active, x.queue, x.latency_ms, x.iops, x.temp) = (o.read_bps, o.write_bps, o.active, o.queue, o.latency_ms, o.iops, o.temp);
            }
        }
        for i in &mut d.net_ifaces {
            if let Some(o) = old_ifaces.iter().find(|o| o.alias == i.alias) {
                (i.rx_bps, i.tx_bps, i.up) = (o.rx_bps, o.tx_bps, o.up);
                (i.ip, i.ssid, i.wifi) = (o.ip.clone(), o.ssid.clone(), o.wifi.clone());
            }
        }
        self.ensure_one_on();
        self.info = Some(info);
    }

    /// Display order of the cores: hardware order, or cluster by cluster with P-cores first.
    fn rebuild_order(&mut self) {
        let n = self.hw_classes.len();
        let mut order: Vec<usize> = (0..n).collect();
        if self.group_pe {
            let (cl, cs) = (&self.hw_clusters, &self.hw_classes);
            order.sort_by_key(|&i| (cl.get(i).cloned().unwrap_or(0), std::cmp::Reverse(cs.get(i).cloned().unwrap_or(0)), i));
        }
        self.data.cpu_classes = order.iter().map(|&i| self.hw_classes.get(i).cloned().unwrap_or(0)).collect();
        self.data.cpu_packages = order.iter().map(|&i| self.hw_clusters.get(i).cloned().unwrap_or(0)).collect();
        self.data.cpu_smt = order.iter().map(|&i| self.hw_smt.get(i).cloned().unwrap_or(false)).collect();
        self.data.cpu_ids = order.iter().map(|&i| i as u16).collect();
        if self.data.cpu_cores.len() == n {
            let cores = self.data.cpu_cores.clone();
            self.data.cpu_cores = order.iter().map(|&i| cores.get(i).cloned().unwrap_or(0.0)).collect();
        }
        self.order = order;
    }

    pub fn set_group_pe(&mut self, on: bool) {
        self.group_pe = on;
        self.rebuild_order();
    }

    fn ensure_one_on(&mut self) {
        let d = &mut self.data;
        if !d.net_ifaces.is_empty() && !d.net_ifaces.iter().any(|i| i.enabled) {
            for i in &mut d.net_ifaces {
                i.enabled = true;
            }
        }
        if !d.disks.is_empty() && !d.disks.iter().any(|x| x.enabled) {
            for x in &mut d.disks {
                x.enabled = true;
            }
        }
    }

    fn apply_snapshot(&mut self, s: Snapshot) {
        let now = Instant::now();
        let dt = self.last_sample.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(1.0).clamp(0.05, 60.0);
        self.last_sample = Some(now);
        let d = &mut self.data;
        // cpu
        d.cpu_total = s.cpu.total.clamp(0.0, 1.0);
        d.cpu_system = s.cpu.sys;
        d.cpu_user = s.cpu.user;
        if !s.cpu.cores.is_empty() {
            let raw: Vec<f32> = s.cpu.cores.iter().map(|c| c.clamp(0.0, 1.0)).collect();
            d.cpu_cores = if self.order.len() == raw.len() { self.order.iter().map(|&i| raw[i]).collect() } else { raw };
            d.cpu_freq = if self.order.len() == s.cpu.freq.len() { self.order.iter().map(|&i| s.cpu.freq[i]).collect() } else { s.cpu.freq.clone() };
            if d.cpu_classes.len() != d.cpu_cores.len() {
                d.cpu_classes = vec![0; d.cpu_cores.len()];
                d.cpu_packages = vec![0; d.cpu_cores.len()];
                d.cpu_smt = vec![false; d.cpu_cores.len()];
                d.cpu_ids = (0..d.cpu_cores.len() as u16).collect();
            }
        }
        metrics::push(&mut d.cpu_history, d.cpu_total);
        d.mark_sample();
        // memory
        d.mem.used = s.mem.used;
        if d.mem.total == 0 {
            d.mem.total = s.mem.used + s.mem.avail;
        }
        d.mem.pct = if d.mem.total > 0 { (d.mem.used as f32 / d.mem.total as f32).clamp(0.0, 1.0) } else { 0.0 };
        // RAM + swap stands in until the agent reports the real commit charge
        if !self.commit_known {
            d.mem.commit_used = s.mem.used + s.mem.swap_used;
        }
        metrics::push(&mut d.mem_history, d.mem.pct);
        // network
        let (mut rx, mut tx) = (0.0, 0.0);
        for n in &s.net {
            if let Some(i) = d.net_ifaces.iter_mut().find(|i| i.alias == n.id) {
                i.rx_bps = n.rx;
                i.tx_bps = n.tx;
                i.up = n.up;
                if i.enabled {
                    rx += n.rx;
                    tx += n.tx;
                }
            }
        }
        d.net_rx = rx;
        d.net_tx = tx;
        d.net_total_rx += (rx * dt) as u64;
        d.net_total_tx += (tx * dt) as u64;
        metrics::push(&mut d.net_history, (rx as f32, tx as f32));
        // disks
        let (mut r, mut w) = (0.0, 0.0);
        for x in &s.disk {
            if let Some(pd) = self.info.as_ref().and_then(|i| i.disks.iter().position(|di| di.id == x.id)).and_then(|p| d.disks.get_mut(p)) {
                pd.read_bps = x.r;
                pd.write_bps = x.w;
                (pd.active, pd.queue, pd.latency_ms, pd.iops, pd.temp) = (x.active, x.queue, x.latency_ms, x.iops, x.temp);
                if pd.enabled {
                    r += x.r;
                    w += x.w;
                }
            }
        }
        d.disk_read = r;
        d.disk_write = w;
        metrics::push(&mut d.disk_history, (r as f32, w as f32));
        // gpu, battery, temperature
        d.gpu = s.gpu.as_ref().map(|g| g.load.clamp(0.0, 1.0));
        if let Some(g) = &s.gpu {
            d.vram_used = g.vram_used;
            d.vram_total = g.vram_total;
            d.vram_budget = g.vram_total;
            if g.vram_total > 0 {
                metrics::push(&mut d.vram_history, (g.vram_used as f64 / g.vram_total as f64).clamp(0.0, 1.0) as f32);
            }
            let p = &mut d.gpu_perf;
            p.temp = g.temp;
            p.hotspot = g.hotspot;
            p.core_mhz = g.core_mhz;
            p.core_max_mhz = g.core_max_mhz;
            p.mem_mhz = g.mem_mhz;
            p.mem_max_mhz = g.mem_max_mhz;
            p.power_w = g.power_w;
            p.power_limit_w = g.power_limit_w;
            p.fan_rpm = g.fan_rpm;
            p.fan_pct = g.fan_pct;
            p.power = match (g.power_w, g.power_limit_w) {
                (Some(w), Some(l)) if l > 0.0 => Some(w / l),
                _ => None,
            };
            (p.pcie_gen, p.pcie_width, p.pcie_max_gen, p.pcie_max_width) = (g.pcie_gen, g.pcie_width, g.pcie_max_gen, g.pcie_max_width);
        }
        metrics::push(&mut d.gpu_history, d.gpu.unwrap_or(0.0));
        d.battery = s.battery.as_ref().map(|b| Battery { level: b.level, charging: b.charging, on_ac: b.on_ac, remaining_secs: b.remaining_secs });
        let mwh = |wh: Option<f32>| wh.map_or(0, |v| (v * 1000.0).max(0.0) as u32);
        d.battery_detail = s.battery.as_ref().filter(|b| b.rate_w.is_some() || b.full_wh.is_some() || b.cycles.is_some()).map(|b| BatteryDetail {
            rate_mw: b.rate_w.map_or(0, |w| (w * 1000.0) as i32),
            remaining_mwh: mwh(b.remaining_wh),
            max_mwh: mwh(b.full_wh),
            design_mwh: mwh(b.design_wh),
            cycles: b.cycles,
        });
        d.cpu_temp = pick_cpu_temp(&s.temp);
        d.load_avg = s.load.clone();
        d.fans = s.fans.iter().map(|f| Fan { id: f.id.clone(), rpm: f.rpm, duty: f.duty }).collect();
        d.power = s.power.iter().map(|p| Power { id: p.id.clone(), w: p.w }).collect();
        // the battery popup's readings and history, as for this machine
        match s.battery.as_ref() {
            Some(b) => {
                let tel = battery_telemetry(b, &s.power);
                if tel.at_unix >= self.hist_at + metrics::power::HIST_STEP {
                    self.hist_at = tel.at_unix;
                    let flow = metrics::powerflow::flow(&tel);
                    let point = metrics::power::HistPoint { at: tel.at_unix, level: b.level, watts: flow.system.value().unwrap_or(f32::NAN), split: metrics::powerflow::split(&tel) };
                    let h = std::sync::Arc::make_mut(&mut d.power_history);
                    h.push(point);
                    if h.len() > metrics::power::HIST_LEN {
                        let extra = h.len() - metrics::power::HIST_LEN;
                        h.drain(..extra);
                    }
                }
                d.power_tel = Some(std::sync::Arc::new(tel));
            }
            None => d.power_tel = None,
        }
    }

    fn apply_details(&mut self, det: Details) {
        let d = &mut self.data;
        let conv = |v: &[ProcSample]| -> Vec<ProcInfo> { v.iter().map(|p| ProcInfo { pid: p.pid, name: p.name.clone(), cpu: p.cpu, mem: p.mem, io: p.io, gpu: p.gpu, vram: p.vram, system: false }).collect() };
        d.top_cpu = conv(&det.top_cpu);
        d.top_mem = conv(&det.top_mem);
        d.top_io = conv(&det.top_io);
        d.top_gpu = conv(&det.top_gpu);
        d.drives = det
            .drives
            .iter()
            .map(|v| Drive { name: v.id.clone(), label: v.label.clone(), kind: kind_static(&v.kind), fs: v.fs.clone(), readonly: v.readonly, free: v.free, total: v.total })
            .collect();
        if let Some(root) = d.drives.iter().find(|v| v.name == "/" || v.name.eq_ignore_ascii_case("C:")).or(d.drives.first()) {
            d.disk_free = root.free;
            d.disk_total = root.total;
        }
        for (id, ip) in &det.ips {
            if let Some(i) = d.net_ifaces.iter_mut().find(|i| i.alias == *id) {
                i.ip = ip.clone();
            }
        }
        for (id, ssid) in &det.ssids {
            if let Some(i) = d.net_ifaces.iter_mut().find(|i| i.alias == *id) {
                i.ssid = ssid.clone();
            }
        }
        for (id, link) in &det.wifi {
            if let Some(i) = d.net_ifaces.iter_mut().find(|i| i.alias == *id) {
                i.wifi = link.clone();
            }
        }
        if let Some(s) = &det.sys {
            let x = &mut d.sys;
            (x.processes, x.threads, x.handles, x.uptime_secs) = (s.processes, s.threads, s.handles, s.uptime_secs);
            (x.ctx_switches, x.interrupts, x.dpc_time, x.interrupt_time) = (s.ctx_switches, s.interrupts, s.dpc_time, s.interrupt_time);
        }
        if let Some(m) = &det.mem {
            d.mem_processes = m.processes;
            d.mem_kernel_paged = m.kernel_paged;
            d.mem_kernel_nonpaged = m.kernel_nonpaged;
            d.mem_cache = m.cache;
            (d.sys.pagefile_used, d.sys.pagefile_total, d.sys.hw_reserved) = (m.swap_used, m.swap_total, m.hw_reserved);
            if m.commit_limit > 0 {
                (d.mem.commit_used, d.mem.commit_total) = (m.commit_used, m.commit_limit);
                self.commit_known = true;
            }
        }
        // busiest interface for the title
        if let Some(b) = d.net_ifaces.iter().filter(|i| i.enabled && i.up).max_by(|a, b| (a.rx_bps + a.tx_bps).partial_cmp(&(b.rx_bps + b.tx_bps)).unwrap_or(std::cmp::Ordering::Equal)) {
            d.net_iface = b.alias.clone();
            d.net_ip = b.ip.clone();
        }
    }

    pub fn set_disabled(&mut self, net: &[String], disks: &[String]) {
        self.net_disabled = net.to_vec();
        self.disks_disabled = disks.to_vec();
        for i in &mut self.data.net_ifaces {
            i.enabled = !self.net_disabled.contains(&i.alias);
        }
        for x in &mut self.data.disks {
            x.enabled = !self.disks_disabled.contains(&x.model);
        }
        self.ensure_one_on();
    }
}

/// Bus names an agent or another winstats may send (PhysicalDisk keeps a static string).
const BUSES: [&str; 21] = ["SSD", "HDD", "USB", "NVMe", "SATA", "SD", "MMC", "SCSI", "ATAPI", "ATA", "IEEE 1394", "SSA", "Fibre", "RAID", "iSCSI", "SAS", "Virtual", "FileBackedVirtual", "Spaces", "SCM", "UFS"];

fn bus_static(s: &str) -> &'static str {
    BUSES.iter().find(|b| **b == s).copied().unwrap_or("")
}

fn adapter(i: usize, g: &winstats_proto::GpuInfo) -> AdapterInfo {
    AdapterInfo { name: g.name.clone(), vendor_id: g.vendor_id, device_id: g.device_id, dedicated: g.vram_total, driver: g.driver.clone(), luid: i as u64 + 1, enabled: true, load: None, perf: Default::default(), dedicated_used: None, shared_used: None, d_state: None }
}

fn kind_static(s: &str) -> &'static str {
    match s {
        "Ethernet" => "Ethernet",
        "Wi-Fi" => "Wi-Fi",
        "Mobile" => "Mobile",
        "Bluetooth" => "Bluetooth",
        "USB" => "USB",
        "DVD" => "DVD",
        _ => "",
    }
}

/// The sensor that most likely is the CPU package.
fn pick_cpu_temp(temps: &[TempSample]) -> Option<f32> {
    let score = |id: &str| -> i32 {
        let l = id.to_ascii_lowercase();
        if l.contains("package") || l.contains("peci") {
            4
        } else if l.contains("cpu") || l.contains("tctl") || l.contains("tdie") {
            3
        } else if l.contains("core") || l.contains("soc") {
            2
        } else {
            0
        }
    };
    temps.iter().max_by_key(|t| score(&t.id)).filter(|t| score(&t.id) > 0).or(temps.first()).map(|t| t.c)
}

/// Everything remote, owned by the App and driven from the UI thread.
pub struct Remotes {
    pub discovery: Option<Discovery>,
    pub agents: BTreeMap<String, Agent>,
}

impl Remotes {
    pub fn new() -> Remotes {
        Remotes { discovery: None, agents: BTreeMap::new() }
    }

    pub fn set_discovery(&mut self, on: bool) {
        if on && self.discovery.is_none() {
            self.discovery = Discovery::start();
        } else if !on {
            self.discovery = None;
        }
    }

    /// Agents heard on the network right now.
    pub fn seen(&self) -> Vec<Seen> {
        self.discovery.as_ref().map(|d| d.seen()).unwrap_or_default()
    }



    pub fn request_details(&self, key: &str, n: usize) {
        if let Some(l) = self.agents.get(key).and_then(|a| a.link.as_ref()) {
            l.state.lock().unwrap().want_details = Some(n);
        }
    }

    pub fn set_group_pe(&mut self, key: &str, on: bool) {
        if let Some(a) = self.agents.get_mut(key) {
            a.set_group_pe(on);
        }
    }

    pub fn set_disabled(&mut self, key: &str, net: &[String], disks: &[String]) {
        if let Some(a) = self.agents.get_mut(key) {
            a.set_disabled(net, disks);
        }
    }

    /// Once per tick: start / stop links to follow the config, refresh addresses from beacons,
    /// drain frames into the data. Returns true when the config was changed (address learned).
    pub fn poll(&mut self, cfg: &mut Config) -> bool {
        let mut changed = false;
        // drop agents that left the config
        self.agents.retain(|k, _| cfg.remotes.contains_key(k));
        for (key, rc) in cfg.remotes.iter_mut() {
            let agent = self.agents.entry(key.clone()).or_insert_with(|| Agent::new(key, rc));
            // learn the address from the beacons. A machine with several interfaces on the network
            // (Wi-Fi and a cable) beacons from each of them: the address in use stays while the
            // machine is heard there, so the link is not restarted with every beacon; another one
            // is tried when the link fails, at most every STALE_SECS
            if let Some(seen) = self.discovery.as_ref().and_then(|d| d.by_id(&rc.id)) {
                let failing = agent.link.as_ref().map_or(false, |l| l.state.lock().unwrap().status == Status::Offline);
                let next = if !seen.heard_from(&rc.address) {
                    Some(seen.addr)
                } else if failing && agent.addr_since.elapsed().as_secs_f64() >= STALE_SECS {
                    seen.other_than(&rc.address)
                } else {
                    None
                };
                if let Some(addr) = next {
                    rc.address = addr.to_string();
                    agent.addr_since = Instant::now();
                    changed = true;
                }
                if rc.name != seen.beacon.name {
                    rc.name = seen.beacon.name.clone();
                    changed = true;
                }
            }
            let want_link = rc.enabled && !rc.address.is_empty();
            match (&agent.link, want_link) {
                (Some(l), true) if l.addr == rc.address && l.token == rc.token => {}
                (_, true) => agent.link = Some(Link::start(rc.address.clone(), rc.token.clone())),
                (Some(_), false) => agent.link = None,
                (None, false) => {}
            }
            let mut frames = Vec::new();
            let mut status = Status::Offline;
            // seconds since the last frame; None = nothing received on this link yet
            let mut silence: Option<f64> = None;
            let mut gone = false;
            if let Some(l) = &agent.link {
                let mut s = l.state.lock().unwrap();
                frames.extend(s.frames.drain(..));
                status = s.status;
                silence = s.last_rx.map(|t| t.elapsed().as_secs_f64());
                gone = s.gone;
            }
            for f in frames {
                match f {
                    Frame::Info(i) => {
                        // a machine added by address: its identity comes with the first answer
                        if rc.id.is_empty() && !i.id.is_empty() {
                            if rc.label.is_empty() || rc.label == default_label(&rc.name) {
                                rc.label = default_label(&i.name);
                            }
                            rc.id = i.id.clone();
                            rc.name = i.name.clone();
                            changed = true;
                        }
                        agent.apply_info(i)
                    }
                    Frame::Snapshot(s) => agent.apply_snapshot(s),
                    Frame::Details(d) => agent.apply_details(d),
                    _ => {}
                }
            }
            // a machine that stops answering fades: its widgets stay in colour for STALE_SECS,
            // then turn grey, and leave the strip after GONE_SECS without data; one that said it
            // is shutting down leaves at once
            let quiet = silence.unwrap_or(f64::MAX);
            let online = !gone && status == Status::Online && quiet < STALE_SECS;
            agent.visible = !gone && quiet < GONE_SECS;
            agent.stale = agent.visible && !online;
            if agent.online && !online {
                agent.offline_since = Some(Instant::now());
            }
            if online {
                agent.offline_since = None;
            }
            agent.online = online;
            agent.data.offline = !online;
        }
        changed
    }

    /// Menu text for an agent's state.
    pub fn state_text(&self, key: &str) -> String {
        match self.agents.get(key) {
            Some(a) if a.online => crate::i18n::t("remote.online"),
            _ => crate::i18n::t("remote.offline"),
        }
    }
}

/// What an agent says about its battery, as the battery popup takes it: the pack, the charger,
/// and the machine's draw ("System" on a Mac) as the platform meter, which lets the popup work
/// out the adapter, battery and system flows; the CPU's own draw as the package meter.
fn battery_telemetry(b: &winstats_proto::BatterySample, power: &[winstats_proto::PowerSample]) -> metrics::power::Telemetry {
    use metrics::power::{Cell, MeterChannel, MeterRole, SourceKind, Telemetry};
    let mwh = |wh: Option<f32>| wh.filter(|v| *v > 0.0).map(|v| (v * 1000.0) as u32);
    let rate_mw = b.rate_w.map(|w| (w * 1000.0) as i32);
    let cell = Cell {
        capacity_mwh: mwh(b.remaining_wh),
        full_mwh: mwh(b.full_wh),
        design_mwh: mwh(b.design_wh),
        voltage_mv: b.voltage_mv,
        rate_mw,
        cycles: b.cycles,
        temp_c: b.temp_c,
        charging: b.charging,
        discharging: rate_mw.map_or(!b.on_ac, |r| r < 0),
        on_line: b.on_ac,
        ..Default::default()
    };
    let meters = power
        .iter()
        .filter_map(|p| {
            let role = match p.id.as_str() {
                "System" => MeterRole::Platform,
                "CPU" => MeterRole::CpuPackage,
                _ => return None,
            };
            Some(MeterChannel { name: p.id.clone(), watts: p.w, role })
        })
        .collect();
    Telemetry {
        at_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        cells: vec![cell],
        level: Some(b.level),
        on_ac: Some(b.on_ac),
        remaining_secs: b.remaining_secs,
        source: b.on_ac.then_some((SourceKind::Ac, b.adapter_ma)),
        source_w: b.adapter_w.filter(|_| b.on_ac),
        meters,
        ..Default::default()
    }
}

/// Three letters from a machine name, the label a newly connected machine starts with.
pub fn default_label(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).take(3).collect::<String>().to_uppercase()
}

/// "host", "host:port", "1.2.3.4", "[::1]:5000" or a bare IPv6 address as typed into the
/// Add by address dialog, as "host:port" with the agent's port when none is given.
/// None for text that cannot be an address.
pub fn normalize_address(text: &str) -> Option<String> {
    let s = text.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    if s.parse::<SocketAddr>().is_ok() {
        return Some(s.to_string());
    }
    if let Ok(ip) = s.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>() {
        return Some(SocketAddr::new(ip, DATA_PORT).to_string());
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') && !host.contains('[') => {
            port.parse::<u16>().ok().filter(|p| *p != 0).map(|_| s.to_string())
        }
        Some(_) => None,
        None if s.contains('[') || s.contains(']') => None,
        None => Some(format!("{s}:{DATA_PORT}")),
    }
}

/// The host part of a "host:port" address.
pub fn address_host(addr: &str) -> &str {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.trim_start_matches('[').trim_end_matches(']')
}

/// Key under `[remotes]` for a newly connected agent: its name, lowercase, safe characters.
pub fn key_for(name: &str, taken: &BTreeMap<String, RemoteCfg>) -> String {
    let mut base: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    base = base.trim_matches('-').to_string();
    if let Some(p) = base.find(".local") {
        base.truncate(p);
    }
    if base.is_empty() {
        base = "agent".into();
    }
    let mut key = base.clone();
    let mut n = 2;
    while taken.contains_key(&key) {
        key = format!("{base}-{n}");
        n += 1;
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(normalize_address(" 10.0.0.5 ").as_deref(), Some("10.0.0.5:47778"));
        assert_eq!(normalize_address("10.0.0.5:5000").as_deref(), Some("10.0.0.5:5000"));
        assert_eq!(normalize_address("box.example.com").as_deref(), Some("box.example.com:47778"));
        assert_eq!(normalize_address("box:47779").as_deref(), Some("box:47779"));
        assert_eq!(normalize_address("fd00::5").as_deref(), Some("[fd00::5]:47778"));
        assert_eq!(normalize_address("[fd00::5]").as_deref(), Some("[fd00::5]:47778"));
        assert_eq!(normalize_address("[fd00::5]:5000").as_deref(), Some("[fd00::5]:5000"));
        assert_eq!(normalize_address(""), None);
        assert_eq!(normalize_address("a b"), None);
        assert_eq!(normalize_address("box:port"), None);
        assert_eq!(normalize_address("box:0"), None);
        assert_eq!(normalize_address(":5000"), None);
        assert_eq!(address_host("box.example.com:47778"), "box.example.com");
        assert_eq!(address_host("[fd00::5]:47778"), "fd00::5");
        assert_eq!(default_label("node1-nv1"), "NOD");
    }
}
