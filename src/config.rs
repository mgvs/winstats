//! User configuration: %APPDATA%\winstats\config.toml

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where the strip goes: "right" (left of the system tray), "left" (left edge of the taskbar)
    pub position: String,
    /// Extra pixels (DIP) to shift the strip away from the edge it is attached to.
    pub offset: i32,
    /// Widgets in display order.
    pub widgets: Vec<String>,
    /// Refresh interval in milliseconds.
    pub update_ms: u32,
    /// Draw a 3-letter vertical label next to chart widgets.
    pub labels: bool,
    /// Colour cpu bars by utilisation (green/yellow/red) instead of a single accent colour.
    pub utilization_colors: bool,
    /// Drive letter for the disk widget free-space readout.
    pub disk: String,
    /// Accent colour: palette name ("system", "blue", "green", ...) or hex like "#2E7CF6".
    pub accent: String,
    /// Per-module colour mode: module (cpu, gpu, mem, disk, net) -> "accent" | "utilization" | "mono" | palette name | hex.
    pub colors: BTreeMap<String, String>,
    /// Force theme: "auto" | "dark" | "light"
    pub theme: String,
    /// Text size multiplier for widgets: 0.85 | 1.0 | 1.15 | 1.3
    pub font_scale: f32,
    /// Show P-cores first, then E-cores, instead of the hardware order Windows uses.
    pub group_pe_cores: bool,
    /// Draw a divider between P and E cores in the bar chart and the popup.
    pub split_pe_cores: bool,
    /// Width of line/network/disk chart widgets in DIP: 34 | 48 | 64 | 96
    pub chart_width: u32,
    /// Width of one core bar in the CPU bar chart, the same for every machine: "small" | "medium" | "large"
    pub bar_width: String,
    /// Rows in the popup process lists: 5 | 10 | 15 | 20
    pub top_processes: u32,
    /// UI language: "auto" (Windows UI language) or a locale code such as "en", "de", "zh-Hant".
    pub language: String,
    /// Network interfaces (by alias, e.g. "Wi-Fi") left out of the widget and the chart.
    pub net_disabled: Vec<String>,
    /// Physical disks (by model) left out of the widget and the chart.
    pub disks_disabled: Vec<String>,
    /// Listen for winstats-agent beacons on the network (Remote menu).
    pub discovery: bool,
    /// Remote machines by key; widgets refer to them as "<key>/<widget>".
    pub remotes: BTreeMap<String, RemoteCfg>,
    /// Act as an agent: announce this machine and serve its metrics to other winstats.
    pub share: bool,
    /// identity used when sharing; generated once
    pub share_id: String,
    /// name announced when sharing; empty = computer name
    pub share_name: String,
    pub share_port: u16,
    /// masters must present this token when sharing is on; empty = anyone on the network
    pub share_token: String,
    /// How the strip is hosted: "auto" (floating on Windows 7, embedded otherwise),
    /// "embedded" (child of the taskbar) or "floating" (always-on-top window over the taskbar).
    pub mode: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: "right".into(),
            offset: 0,
            widgets: vec![
                "cpu_bars".into(),
                "cpu_line".into(),
                "mem".into(),
                "gpu".into(),
                "net".into(),
                "disk".into(),
                "battery".into(),
            ],
            update_ms: 1000,
            labels: true,
            utilization_colors: false,
            disk: "C:".into(),
            accent: "blue".into(),
            colors: BTreeMap::new(),
            theme: "auto".into(),
            font_scale: 1.0,
            group_pe_cores: false,
            split_pe_cores: true,
            chart_width: 34,
            bar_width: "small".into(),
            mode: "auto".into(),
            top_processes: 5,
            language: "auto".into(),
            net_disabled: Vec::new(),
            disks_disabled: Vec::new(),
            discovery: false,
            remotes: BTreeMap::new(),
            share: false,
            share_id: String::new(),
            share_name: String::new(),
            share_port: 47778,
            share_token: String::new(),
        }
    }
}

/// One remote machine (a winstats-agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteCfg {
    /// the agent's installation id from its beacon; the binding survives renames and new IPs
    pub id: String,
    /// last name the agent announced
    pub name: String,
    /// "host:port"; learned from the beacon, or typed in for machines discovery cannot reach
    pub address: String,
    pub enabled: bool,
    /// vertical label next to this machine's widgets (up to 3 characters)
    pub label: String,
    pub token: String,
    /// colour mode per module, like `colors`
    pub colors: BTreeMap<String, String>,
    pub net_disabled: Vec<String>,
    pub disks_disabled: Vec<String>,
    /// vertical label next to this machine's widgets on / off (the global `labels` still rules)
    pub labels: bool,
    /// divider between P and E cores (and clusters) in this machine's bar chart
    pub split_pe: bool,
    /// P-cores first, then E-cores, instead of the hardware order the agent reports
    pub group_pe: bool,
}

impl Default for RemoteCfg {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            address: String::new(),
            enabled: false,
            label: String::new(),
            token: String::new(),
            colors: BTreeMap::new(),
            net_disabled: Vec::new(),
            disks_disabled: Vec::new(),
            labels: true,
            split_pe: true,
            group_pe: false,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        crate::util::config_dir().join("config.toml")
    }

    /// Load the config; write a default file if none exists.
    pub fn load() -> Config {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(c) => c,
                Err(e) => {
                    crate::log!("config parse error: {e}; using defaults");
                    Config::default()
                }
            },
            Err(_) => {
                let c = Config::default();
                c.save();
                c
            }
        }
    }

    /// Write the config back (comments in the file are not preserved).
    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(s) => {
                let header = "# winstats configuration\n# widgets: cpu_bars, cpu_line, cpu_mini, gpu, gpu_line, mem, mem_line,\n#          disk, disk_chart, net, net_chart, battery\n\n";
                if let Err(e) = std::fs::write(&path, format!("{header}{s}")) {
                    crate::log!("config save failed: {e}");
                }
            }
            Err(e) => crate::log!("config serialize failed: {e}"),
        }
    }

    pub fn accent_color(&self) -> crate::canvas::Color {
        crate::widgets::named_color(&self.accent).unwrap_or(crate::widgets::BLUE)
    }
}
