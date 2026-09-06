//! `install-service` / `uninstall-service`: start the agent at boot.
//!
//! Linux: a systemd unit. As root a system unit in /etc/systemd/system (runs at boot, no login
//! needed); as a user a user unit plus `loginctl enable-linger` so it starts at boot too.
//! macOS: a LaunchAgent plist in ~/Library/LaunchAgents, loaded with launchctl.
//! Windows: a Run key in HKCU (winstats itself can also share the machine, which needs no agent).

use std::path::{Path, PathBuf};
use std::process::Command;

const NAME: &str = "winstats-agent";

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(NAME))
}

fn run(cmd: &str, args: &[&str]) -> bool {
    match Command::new(cmd).args(args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("{cmd} {}: exit {}", args.join(" "), s.code().unwrap_or(-1));
            false
        }
        Err(e) => {
            eprintln!("{cmd}: {e}");
            false
        }
    }
}

#[allow(dead_code)]
fn write(path: &Path, text: &str) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(path, text) {
        Ok(()) => {
            println!("wrote {}", path.display());
            true
        }
        Err(e) => {
            eprintln!("cannot write {}: {e}", path.display());
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status").map(|s| s.lines().any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))).unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub fn install(config: &Path) {
    let exe = exe();
    let unit = format!(
        "[Unit]\nDescription=winstats agent: resource usage for winstats on the network\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={} --config {}\nRestart=always\nRestartSec=3\nNice=10\n\n[Install]\nWantedBy={}\n",
        exe.display(),
        config.display(),
        if is_root() { "multi-user.target" } else { "default.target" }
    );
    if is_root() {
        let path = PathBuf::from(format!("/etc/systemd/system/{NAME}.service"));
        if !write(&path, &unit) {
            return;
        }
        run("systemctl", &["daemon-reload"]);
        if run("systemctl", &["enable", "--now", NAME]) {
            println!("installed as a system service: systemctl status {NAME}");
        }
    } else {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let path = home.join(".config/systemd/user").join(format!("{NAME}.service"));
        if !write(&path, &unit) {
            return;
        }
        run("systemctl", &["--user", "daemon-reload"]);
        if run("systemctl", &["--user", "enable", "--now", NAME]) {
            println!("installed as a user service: systemctl --user status {NAME}");
        }
        // keep the user's services running without a login session (needed at boot)
        if let Ok(user) = std::env::var("USER") {
            if !run("loginctl", &["enable-linger", &user]) {
                println!("note: `sudo loginctl enable-linger {user}` lets it start at boot without a login; or run install-service with sudo for a system service");
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn uninstall() {
    if is_root() {
        run("systemctl", &["disable", "--now", NAME]);
        let _ = std::fs::remove_file(format!("/etc/systemd/system/{NAME}.service"));
        run("systemctl", &["daemon-reload"]);
    } else {
        run("systemctl", &["--user", "disable", "--now", NAME]);
        if let Some(home) = std::env::var_os("HOME") {
            let _ = std::fs::remove_file(PathBuf::from(home).join(".config/systemd/user").join(format!("{NAME}.service")));
        }
        run("systemctl", &["--user", "daemon-reload"]);
    }
    println!("removed");
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/LaunchAgents/org.mgvs.winstats-agent.plist")
}

#[cfg(target_os = "macos")]
pub fn install(config: &Path) {
    let exe = exe();
    let log = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Library/Logs/winstats-agent.log");
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>org.mgvs.winstats-agent</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>--config</string>\n    <string>{}</string>\n  </array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ProcessType</key><string>Background</string>\n  <key>StandardErrorPath</key><string>{}</string>\n  <key>StandardOutPath</key><string>{}</string>\n</dict>\n</plist>\n",
        exe.display(),
        config.display(),
        log.display(),
        log.display()
    );
    let path = plist_path();
    let _ = run("launchctl", &["unload", &path.to_string_lossy()]);
    if !write(&path, &plist) {
        return;
    }
    if run("launchctl", &["load", "-w", &path.to_string_lossy()]) {
        println!("installed as a LaunchAgent (starts at login); log: {}", log.display());
    }
}

#[cfg(target_os = "macos")]
pub fn uninstall() {
    let path = plist_path();
    run("launchctl", &["unload", "-w", &path.to_string_lossy()]);
    let _ = std::fs::remove_file(&path);
    println!("removed");
}

#[cfg(windows)]
pub fn install(config: &Path) {
    let exe = exe();
    let value = format!("cmd /c start /min \"\" \"{}\" --config \"{}\"", exe.display(), config.display());
    if run("reg", &["add", "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run", "/v", NAME, "/t", "REG_SZ", "/d", &value, "/f"]) {
        println!("registered to start at logon (minimised console). On Windows, winstats itself can share the machine instead: Remote > Share this machine.");
    }
}

#[cfg(windows)]
pub fn uninstall() {
    run("reg", &["delete", "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run", "/v", NAME, "/f"]);
    println!("removed");
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn install(_config: &Path) {
    eprintln!("no service installer for this OS; start {} from your init system", exe().display());
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn uninstall() {}
