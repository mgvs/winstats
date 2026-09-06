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
use crate::metrics::{self, battery::Battery, disks::PhysicalDisk, drives::Drive, net::NetIface, processes::ProcInfo, MetricsData};

/// No data for this long: the machine's widgets turn grey.
pub const STALE_SECS: f64 = 10.0;
/// No data for this long: the machine's widgets leave the strip (they come back with the data).
pub const GONE_SECS: f64 = 30.0;

/// An agent heard on the network.
#[derive(Clone, Debug)]
pub struct Seen {
    pub beacon: Beacon,
    pub addr: SocketAddr,
    pub at: Instant,
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
                        seen.lock().unwrap().insert(b.id.clone(), Seen { beacon: b, addr, at: Instant::now() });
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
}

/// One TCP session with an agent, reconnecting with backoff until dropped.
pub struct Link {
    state: Arc<Mutex<LinkState>>,
    stop: Arc<AtomicBool>,
    pub addr: String,
}

impl Link {
    fn start(addr: String, token: String) -> Link {
        let state = Arc::new(Mutex::new(LinkState { status: Status::Connecting, frames: VecDeque::new(), want_details: None, last_rx: None }));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let state = state.clone();
            let stop = stop.clone();
            let addr = addr.clone();
            std::thread::Builder::new().name(format!("link {addr}")).spawn(move || link_thread(addr, token, state, stop)).ok();
        }
        Link { state, stop, addr }
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
        match session(&addr, &token, &state, &stop) {
            Ok(()) => backoff = 1,
            Err(_) => state.lock().unwrap().status = Status::Offline,
        }
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
            Ok(Frame::Bye { reason }) => break Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, reason)),
            Ok(Frame::Pong) => {
                state.lock().unwrap().last_rx = Some(Instant::now());
            }
            Ok(frame) => {
                let mut s = state.lock().unwrap();
                s.status = Status::Online;
                s.last_rx = Some(Instant::now());
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
    /// still drawn: data arrived within `GONE_SECS`
    pub visible: bool,
    /// drawn in grey: no data for `STALE_SECS`, not yet gone
    pub stale: bool,
    /// when the link went down, for the popup
    pub offline_since: Option<Instant>,
    net_disabled: Vec<String>,
    disks_disabled: Vec<String>,
    last_sample: Option<Instant>,
    /// P-cores first, then E-cores (Group P / E cores in the menu)
    group_pe: bool,
    /// hardware order of the cores as the agent reports them
    hw_classes: Vec<u8>,
    hw_clusters: Vec<u8>,
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
            net_disabled: cfg.net_disabled.clone(),
            disks_disabled: cfg.disks_disabled.clone(),
            last_sample: None,
            group_pe: cfg.group_pe,
            hw_classes: Vec::new(),
            hw_clusters: Vec::new(),
            order: Vec::new(),
        }
    }

    fn apply_info(&mut self, info: Info) {
        let d = &mut self.data;
        d.host = info.name.clone();
        d.cpu_name = info.cpu_name.clone();
        self.hw_classes = if info.classes.len() == info.cores { info.classes.clone() } else { vec![0; info.cores] };
        // clusters give the dividers in the core chart; the socket count is separate
        self.hw_clusters = if info.clusters.len() == info.cores { info.clusters.clone() } else { vec![0; info.cores] };
        let d = &mut self.data;
        d.cpu_sockets = info.sockets.max(1);
        d.cpu_cores = vec![0.0; info.cores];
        self.rebuild_order();
        let d = &mut self.data;
        d.mem.total = info.mem_total;
        d.mem.commit_total = info.mem_total + info.swap_total;
        d.gpu_available = info.has.iter().any(|h| h == "gpu");
        d.battery_available = info.has.iter().any(|h| h == "battery");
        d.disks = info
            .disks
            .iter()
            .enumerate()
            .map(|(i, x)| PhysicalDisk { index: i as u32, model: x.model.clone(), bus: bus_static(&x.bus), size: x.size, read_bps: 0.0, write_bps: 0.0, enabled: !self.disks_disabled.contains(&x.model) })
            .collect();
        d.net_ifaces = info
            .ifaces
            .iter()
            .map(|x| NetIface {
                luid: 0,
                guid: Default::default(),
                alias: x.id.clone(),
                description: x.mac.clone(),
                kind: kind_static(&x.kind),
                up: true,
                rx_bps: 0.0,
                tx_bps: 0.0,
                ip: String::new(),
                ssid: String::new(),
                enabled: !self.net_disabled.contains(&x.id),
            })
            .collect();
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
            }
        }
        metrics::push(&mut d.cpu_history, d.cpu_total);
        // memory
        d.mem.used = s.mem.used;
        if d.mem.total == 0 {
            d.mem.total = s.mem.used + s.mem.avail;
        }
        d.mem.pct = if d.mem.total > 0 { (d.mem.used as f32 / d.mem.total as f32).clamp(0.0, 1.0) } else { 0.0 };
        d.mem.commit_used = s.mem.used + s.mem.swap_used;
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
        }
        metrics::push(&mut d.gpu_history, d.gpu.unwrap_or(0.0));
        d.battery = s.battery.as_ref().map(|b| Battery { level: b.level, charging: b.charging, on_ac: b.on_ac, remaining_secs: b.remaining_secs });
        d.cpu_temp = pick_cpu_temp(&s.temp);
        d.load_avg = s.load.clone();
    }

    fn apply_details(&mut self, det: Details) {
        let d = &mut self.data;
        let conv = |v: &[ProcSample]| -> Vec<ProcInfo> { v.iter().map(|p| ProcInfo { pid: p.pid, name: p.name.clone(), cpu: p.cpu, mem: p.mem, io: p.io }).collect() };
        d.top_cpu = conv(&det.top_cpu);
        d.top_mem = conv(&det.top_mem);
        d.top_io = conv(&det.top_io);
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

fn bus_static(s: &str) -> &'static str {
    match s {
        "SSD" => "SSD",
        "HDD" => "HDD",
        "USB" => "USB",
        "NVMe" => "NVMe",
        "SATA" => "SATA",
        "SD" => "SD",
        _ => "",
    }
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
            // learn the address from the beacon
            if let Some(seen) = self.discovery.as_ref().and_then(|d| d.by_id(&rc.id)) {
                let addr = seen.addr.to_string();
                if rc.address != addr {
                    rc.address = addr;
                    changed = true;
                }
                if rc.name != seen.beacon.name {
                    rc.name = seen.beacon.name.clone();
                    changed = true;
                }
            }
            let agent = self.agents.entry(key.clone()).or_insert_with(|| Agent::new(key, rc));
            let want_link = rc.enabled && !rc.address.is_empty();
            match (&agent.link, want_link) {
                (Some(l), true) if l.addr == rc.address => {}
                (_, true) => agent.link = Some(Link::start(rc.address.clone(), rc.token.clone())),
                (Some(_), false) => agent.link = None,
                (None, false) => {}
            }
            let mut frames = Vec::new();
            let mut status = Status::Offline;
            // seconds since the last frame; None = nothing received on this link yet
            let mut silence: Option<f64> = None;
            if let Some(l) = &agent.link {
                let mut s = l.state.lock().unwrap();
                frames.extend(s.frames.drain(..));
                status = s.status;
                silence = s.last_rx.map(|t| t.elapsed().as_secs_f64());
            }
            for f in frames {
                match f {
                    Frame::Info(i) => agent.apply_info(i),
                    Frame::Snapshot(s) => agent.apply_snapshot(s),
                    Frame::Details(d) => agent.apply_details(d),
                    _ => {}
                }
            }
            // a machine that stops answering fades: its widgets stay in colour for STALE_SECS,
            // then turn grey, and leave the strip after GONE_SECS without data
            let quiet = silence.unwrap_or(f64::MAX);
            let online = status == Status::Online && quiet < STALE_SECS;
            agent.visible = quiet < GONE_SECS;
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
