//! winstats-agent: broadcasts this machine's resource usage to winstats on the LAN.
//!
//!   winstats-agent                 run (beacon + TCP server)
//!   winstats-agent --dump          print one snapshot as JSON and exit
//!   winstats-agent discover [secs] listen for beacons and print them
//!   winstats-agent watch HOST[:PORT] [--token T]   connect like a master and print frames
//!   winstats-agent install-service | uninstall-service   start at boot (systemd / launchd / Run key)

mod collect;
mod config;
mod net;
mod service;
#[cfg(windows)]
mod wincpu;

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use config::Config;
use winstats_proto::*;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Latest snapshot, shared between the sampler and every client writer.
pub struct Shared {
    pub snap: Mutex<Snapshot>,
    pub changed: Condvar,
    pub info: Info,
    pub collector: Mutex<collect::Collector>,
    pub cfg: Config,
}

fn usage() -> ! {
    eprintln!("{}", "winstats-agent [options]\n  --config PATH     agent.toml (default: per-user config dir)\n  --name NAME       name shown in winstats\n  --port PORT       TCP port (default 47778)\n  --interval MS     sample interval\n  --token TOKEN     masters must present it\n  --no-multicast    no beacons, masters add this agent by address\n  --dump            print one snapshot and exit\n  discover [SECS]   print beacons heard on the network\n  watch HOST[:PORT] [--token T]   connect and print frames\n  install-service   start at boot (systemd unit, LaunchAgent, Run key); uninstall-service removes it");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg_path: Option<PathBuf> = None;
    let mut overrides = Config::default();
    let mut set_name = false;
    let mut set_port = false;
    let mut set_interval = false;
    let mut set_token = false;
    let mut no_multicast = false;
    let mut dump = false;
    let mut mode: Option<(String, Vec<String>)> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_else(|| usage())
        };
        match a {
            "--config" => cfg_path = Some(PathBuf::from(next(&mut i))),
            "--name" => {
                overrides.name = next(&mut i);
                set_name = true;
            }
            "--port" => {
                overrides.port = next(&mut i).parse().unwrap_or_else(|_| usage());
                set_port = true;
            }
            "--interval" => {
                overrides.interval_ms = next(&mut i).parse().unwrap_or_else(|_| usage());
                set_interval = true;
            }
            "--token" => {
                overrides.token = next(&mut i);
                set_token = true;
            }
            "--no-multicast" => no_multicast = true,
            "--dump" => dump = true,
            "-h" | "--help" => usage(),
            "discover" | "watch" | "install-service" | "uninstall-service" => {
                mode = Some((a.to_string(), args[i + 1..].to_vec()));
                break;
            }
            _ => usage(),
        }
        i += 1;
    }

    if let Some((m, rest)) = mode {
        match m.as_str() {
            "discover" => {
                let secs = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10u64);
                net::discover(secs);
            }
            "install-service" => {
                let path = cfg_path.unwrap_or_else(Config::default_path);
                // make sure the config (and the id) exists before the service starts
                let _ = Config::load(&path);
                service::install(&path);
            }
            "uninstall-service" => service::uninstall(),
            _ => {
                let host = rest.first().cloned().unwrap_or_else(|| usage());
                let token = rest.iter().position(|s| s == "--token").and_then(|p| rest.get(p + 1)).cloned().unwrap_or_default();
                net::watch(&host, &token);
            }
        }
        return;
    }

    let path = cfg_path.unwrap_or_else(Config::default_path);
    let mut cfg = Config::load(&path);
    if set_name {
        cfg.name = overrides.name;
    }
    if set_port {
        cfg.port = overrides.port;
    }
    if set_interval {
        cfg.interval_ms = overrides.interval_ms;
    }
    if set_token {
        cfg.token = overrides.token;
    }
    if no_multicast {
        cfg.multicast = false;
    }
    cfg.interval_ms = cfg.interval_ms.clamp(250, 60_000);

    let mut collector = collect::Collector::new(&cfg.id, &cfg.name, AGENT_VERSION);
    if dump {
        std::thread::sleep(Duration::from_millis(500));
        let snap = collector.sample();
        let det = collector.details(5);
        println!("{}", serde_json::to_string_pretty(&collector.info).unwrap());
        println!("{}", serde_json::to_string_pretty(&snap).unwrap());
        println!("{}", serde_json::to_string_pretty(&det).unwrap());
        return;
    }

    let info = collector.info.clone();
    eprintln!("winstats-agent {AGENT_VERSION}: {} ({} {}), {} cores, port {}, multicast {}, config {}", info.name, info.os, info.arch, info.cores, cfg.port, if cfg.multicast { "on" } else { "off" }, path.display());

    let shared = Arc::new(Shared { snap: Mutex::new(Snapshot::default()), changed: Condvar::new(), info, collector: Mutex::new(collector), cfg: cfg.clone() });

    // sampler
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("sampler".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(shared.cfg.interval_ms));
                let snap = shared.collector.lock().unwrap().sample();
                *shared.snap.lock().unwrap() = snap;
                shared.changed.notify_all();
            })
            .expect("sampler thread");
    }
    if cfg.multicast {
        let shared = shared.clone();
        std::thread::Builder::new().name("beacon".into()).spawn(move || net::beacon_loop(shared)).expect("beacon thread");
    }
    net::serve(shared);
}
