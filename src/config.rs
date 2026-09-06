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
    /// Rows in the popup process lists: 5 | 10 | 15 | 20
    pub top_processes: u32,
    /// UI language: "auto" (Windows UI language) or a locale code such as "en", "de", "zh-Hant".
    pub language: String,
    /// Network interfaces (by alias, e.g. "Wi-Fi") left out of the widget and the chart.
    pub net_disabled: Vec<String>,
    /// Physical disks (by model) left out of the widget and the chart.
    pub disks_disabled: Vec<String>,
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
            mode: "auto".into(),
            top_processes: 5,
            language: "auto".into(),
            net_disabled: Vec::new(),
            disks_disabled: Vec::new(),
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
