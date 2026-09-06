//! agent.toml: written with defaults and a fresh id on the first run.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// stable identity of this installation; generated once
    pub id: String,
    /// shown in the master's menu; empty = host name
    pub name: String,
    /// TCP port of the data stream
    pub port: u16,
    /// UDP multicast / broadcast beacons on and off
    pub multicast: bool,
    pub interval_ms: u64,
    /// masters must present the same token; empty = anyone on the network
    pub token: String,
    /// address to bind the TCP listener to
    pub bind: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            port: winstats_proto::DATA_PORT,
            multicast: true,
            interval_ms: 1000,
            token: String::new(),
            bind: "0.0.0.0".into(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        #[cfg(windows)]
        {
            let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            base.join("winstats-agent").join("agent.toml")
        }
        #[cfg(not(windows))]
        {
            if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
                return PathBuf::from(dir).join("winstats-agent").join("agent.toml");
            }
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            home.join(".config").join("winstats-agent").join("agent.toml")
        }
    }

    /// Load, or create with defaults. The id is generated and written back when missing.
    pub fn load(path: &PathBuf) -> Config {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{}: {e}; using defaults", path.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        };
        if cfg.id.is_empty() {
            cfg.id = new_id();
            cfg.save(path);
        }
        cfg
    }

    pub fn save(&self, path: &PathBuf) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(s) => {
                let header = "# winstats-agent configuration\n\n";
                if let Err(e) = std::fs::write(path, format!("{header}{s}")) {
                    eprintln!("cannot write {}: {e}", path.display());
                }
            }
            Err(e) => eprintln!("config serialize failed: {e}"),
        }
    }
}

/// 128 random bits as 32 hex digits. `RandomState` is seeded from the OS per process, which
/// is all the randomness an installation id needs; no extra dependency.
fn new_id() -> String {
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
