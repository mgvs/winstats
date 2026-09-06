//! Beacons (UDP multicast + subnet broadcast), the TCP server, and the two debugging client
//! modes (`discover`, `watch`).

use std::io::{BufReader, BufWriter};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::Shared;
use winstats_proto::*;

/// One beacon per interface: a socket bound to each local IPv4 sends to the multicast group and
/// a directed broadcast to that interface's subnet, so a master on any segment the agent sits
/// on hears it (the default route alone misses the others, e.g. a Mac with two ports).
pub fn beacon_loop(shared: Arc<Shared>) {
    let mcast: SocketAddr = format!("{MULTICAST_ADDR}:{BEACON_PORT}").parse().unwrap();
    let fallback = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = &fallback {
        let _ = s.set_multicast_ttl_v4(1);
        let _ = s.set_broadcast(true);
    }
    let mut socks: Vec<(Ipv4Addr, u8, UdpSocket)> = Vec::new();
    let mut ips_known: Vec<(Ipv4Addr, u8)> = Vec::new();
    loop {
        let (cpu, mem, ips) = {
            let c = shared.collector.lock().unwrap();
            let (cpu, mem) = c.quick();
            (cpu, mem, c.local_ipv4s())
        };
        if ips != ips_known {
            ips_known = ips.clone();
            socks.clear();
            for (ip, prefix) in &ips {
                if let Ok(s) = UdpSocket::bind((*ip, 0)) {
                    let _ = s.set_multicast_ttl_v4(1);
                    let _ = s.set_broadcast(true);
                    socks.push((*ip, *prefix, s));
                }
            }
        }
        let b = Beacon {
            v: VERSION,
            id: shared.info.id.clone(),
            name: shared.info.name.clone(),
            os: shared.info.os.clone(),
            arch: shared.info.arch.clone(),
            port: shared.cfg.port,
            agent: crate::AGENT_VERSION.to_string(),
            cpu,
            mem,
        };
        if let Ok(body) = serde_json::to_vec(&b) {
            for (ip, prefix, s) in &socks {
                let _ = s.send_to(&body, mcast);
                // directed broadcast of this interface's subnet
                let mask = if *prefix == 0 { 0 } else { u32::MAX << (32 - *prefix as u32) };
                let bcast = Ipv4Addr::from(u32::from(*ip) | !mask);
                let _ = s.send_to(&body, SocketAddr::from((bcast, BEACON_PORT)));
            }
            if socks.is_empty() {
                if let Some(s) = &fallback {
                    let _ = s.send_to(&body, mcast);
                    let _ = s.send_to(&body, SocketAddr::from((Ipv4Addr::BROADCAST, BEACON_PORT)));
                }
            }
        }
        std::thread::sleep(Duration::from_secs(BEACON_INTERVAL_SECS));
    }
}

pub fn serve(shared: Arc<Shared>) {
    let addr = format!("{}:{}", shared.cfg.bind, shared.cfg.port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let shared = shared.clone();
                let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                std::thread::Builder::new()
                    .name(format!("client {peer}"))
                    .spawn(move || {
                        if let Err(e) = client(s, shared) {
                            eprintln!("{peer}: {e}");
                        }
                    })
                    .ok();
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
}

/// One master: hello / info handshake, then a writer thread streams snapshots while this
/// thread answers requests.
fn client(stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(PING_INTERVAL_SECS * 6)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let writer = Arc::new(Mutex::new(BufWriter::new(stream)));

    let hello = read_frame(&mut reader)?;
    match hello {
        Frame::Hello { v, client, token } => {
            if v != VERSION {
                write_frame(&mut *writer.lock().unwrap(), &Frame::Bye { reason: format!("protocol {v}, agent speaks {VERSION}") })?;
                return Ok(());
            }
            if !shared.cfg.token.is_empty() && token != shared.cfg.token {
                write_frame(&mut *writer.lock().unwrap(), &Frame::Bye { reason: "bad token".into() })?;
                return Ok(());
            }
            eprintln!("{peer}: connected ({client})");
        }
        _ => {
            write_frame(&mut *writer.lock().unwrap(), &Frame::Bye { reason: "hello expected".into() })?;
            return Ok(());
        }
    }
    write_frame(&mut *writer.lock().unwrap(), &Frame::Info(shared.info.clone()))?;

    // writer: every new sample goes out; stops when the reader has closed the socket
    let alive = Arc::new(Mutex::new(true));
    {
        let shared = shared.clone();
        let writer = writer.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut last_seq = 0u64;
            loop {
                let snap = {
                    let guard = shared.snap.lock().unwrap();
                    let (guard, _) = shared.changed.wait_timeout(guard, Duration::from_secs(2)).unwrap();
                    guard.clone()
                };
                if !*alive.lock().unwrap() {
                    break;
                }
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
        match read_frame(&mut reader) {
            Ok(Frame::Ping) => {
                if let Err(e) = write_frame(&mut *writer.lock().unwrap(), &Frame::Pong) {
                    break Err(e);
                }
            }
            Ok(Frame::Details(req)) => {
                let det = shared.collector.lock().unwrap().details(req.n);
                if let Err(e) = write_frame(&mut *writer.lock().unwrap(), &Frame::Details(det)) {
                    break Err(e);
                }
            }
            Ok(Frame::Bye { .. }) => break Ok(()),
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    *alive.lock().unwrap() = false;
    shared.changed.notify_all();
    eprintln!("{peer}: disconnected");
    result
}

/// Print every beacon heard for `secs` seconds.
pub fn discover(secs: u64) {
    let sock = open_beacon_socket(&[]).expect("beacon socket");
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
    let end = Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; 2048];
    println!("listening for beacons on {MULTICAST_ADDR}:{BEACON_PORT} for {secs} s");
    while Instant::now() < end {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            match serde_json::from_slice::<Beacon>(&buf[..n]) {
                Ok(b) => println!("{from}  {} ({} {}) port {} cpu {:.0}% mem {:.0}% agent {} id {}", b.name, b.os, b.arch, b.port, b.cpu * 100.0, b.mem * 100.0, b.agent, b.id),
                Err(e) => println!("{from}: bad beacon: {e}"),
            }
        }
    }
}

/// Connect like a master and print what arrives.
pub fn watch(host: &str, token: &str) {
    let target = if host.contains(':') { host.to_string() } else { format!("{host}:{DATA_PORT}") };
    let addr = target.to_socket_addrs().ok().and_then(|mut a| a.next()).unwrap_or_else(|| {
        eprintln!("cannot resolve {target}");
        std::process::exit(1)
    });
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap_or_else(|e| {
        eprintln!("{addr}: {e}");
        std::process::exit(1)
    });
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);
    write_frame(&mut writer, &Frame::Hello { v: VERSION, client: format!("winstats-agent watch {}", crate::AGENT_VERSION), token: token.to_string() }).unwrap();
    let mut last_ping = Instant::now();
    let mut asked = false;
    loop {
        match read_frame(&mut reader) {
            Ok(Frame::Info(i)) => println!("info: {} {} {} ({}), {} cores, {} MB, disks {:?}, ifaces {:?}", i.name, i.os, i.os_version, i.arch, i.cores, i.mem_total / 1_048_576, i.disks.iter().map(|d| &d.id).collect::<Vec<_>>(), i.ifaces.iter().map(|d| &d.id).collect::<Vec<_>>()),
            Ok(Frame::Snapshot(s)) => {
                let rx: f64 = s.net.iter().map(|n| n.rx).sum();
                let tx: f64 = s.net.iter().map(|n| n.tx).sum();
                let r: f64 = s.disk.iter().map(|d| d.r).sum();
                let w: f64 = s.disk.iter().map(|d| d.w).sum();
                let temp = s.temp.first().map(|t| format!(" {}={:.0}C", t.id, t.c)).unwrap_or_default();
                println!("#{} cpu {:>3.0}% [{}] mem {} MB net {:.0}/{:.0} KB/s disk {:.0}/{:.0} KB/s load {:?}{}", s.seq, s.cpu.total * 100.0, s.cpu.cores.iter().map(|c| format!("{:.0}", c * 100.0)).collect::<Vec<_>>().join(" "), s.mem.used / 1_048_576, rx / 1024.0, tx / 1024.0, r / 1024.0, w / 1024.0, s.load, temp);
                if !asked {
                    asked = true;
                    write_frame(&mut writer, &Frame::Details(Details { n: 5, ..Default::default() })).unwrap();
                }
            }
            Ok(Frame::Details(d)) => {
                println!("details: top cpu {:?}", d.top_cpu.iter().map(|p| format!("{} {:.1}%", p.name, p.cpu * 100.0)).collect::<Vec<_>>());
                println!("         top mem {:?}", d.top_mem.iter().map(|p| format!("{} {} MB", p.name, p.mem / 1_048_576)).collect::<Vec<_>>());
                println!("         drives {:?}", d.drives.iter().map(|v| format!("{} {} {} GB free", v.id, v.fs, v.free / 1_073_741_824)).collect::<Vec<_>>());
                println!("         ips {:?}", d.ips);
            }
            Ok(Frame::Pong) => {}
            Ok(Frame::Bye { reason }) => {
                println!("bye: {reason}");
                break;
            }
            Ok(other) => println!("{other:?}"),
            Err(e) => {
                println!("link lost: {e}");
                break;
            }
        }
        if last_ping.elapsed() >= Duration::from_secs(PING_INTERVAL_SECS) {
            last_ping = Instant::now();
            let _ = write_frame(&mut writer, &Frame::Ping);
        }
    }
}
