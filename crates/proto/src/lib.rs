//! Wire types shared by winstats (the master) and winstats-agent.
//!
//! Discovery: agents multicast a [`Beacon`] as one JSON datagram to [`MULTICAST_ADDR`]:[`BEACON_PORT`]
//! every [`BEACON_INTERVAL_SECS`] seconds.
//!
//! Data: TCP on the agent's `port`, every frame is a `u32` little-endian length followed by one
//! JSON-encoded [`Frame`]. The master sends [`Frame::Hello`] first, the agent answers with
//! [`Frame::Info`] and then streams [`Frame::Snapshot`] once per interval; [`Frame::Details`] is
//! request/response while a popup is open; [`Frame::Ping`]/[`Frame::Pong`] detect a dead link.
//!
//! Unknown fields are ignored on both sides, new optional fields are added without bumping
//! [`VERSION`]; the version changes only on an incompatible change.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, UdpSocket};

pub const VERSION: u32 = 1;
pub const MULTICAST_ADDR: &str = "239.255.77.77";
pub const BEACON_PORT: u16 = 47777;
pub const DATA_PORT: u16 = 47778;
pub const BEACON_INTERVAL_SECS: u64 = 2;
/// an agent that has not been heard from for this long is considered gone
pub const BEACON_TIMEOUT_SECS: u64 = 10;
pub const PING_INTERVAL_SECS: u64 = 5;
/// upper bound for one frame, everything bigger is a protocol error
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// Presence announcement, one datagram, no reply.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Beacon {
    pub v: u32,
    /// stable per agent installation
    pub id: String,
    pub name: String,
    pub os: String,
    pub arch: String,
    /// TCP port of the data stream
    pub port: u16,
    pub agent: String,
    /// current load, 0..1, so the master can show it before connecting
    pub cpu: f32,
    pub mem: f32,
}

/// One logical CPU's efficiency class (0 = the efficient / only class, higher = faster).
pub type CoreClass = u8;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DiskInfo {
    /// stable id on the agent ("nvme0n1", "disk0", "PhysicalDrive0")
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub bus: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IfaceInfo {
    /// interface name ("eth0", "en0", "Wi-Fi")
    pub id: String,
    /// "Ethernet", "Wi-Fi", "Mobile", "Bluetooth" or ""
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub mac: String,
}

/// What the agent is and what it can report; sent once after `hello`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub v: u32,
    pub id: String,
    pub name: String,
    pub os: String,
    /// "Ubuntu 22.04", "macOS 15.7", "Windows 11"
    #[serde(default)]
    pub os_version: String,
    pub arch: String,
    pub agent: String,
    #[serde(default)]
    pub cpu_name: String,
    pub cores: usize,
    /// per logical core, same length as `cores` (all zero when unknown)
    #[serde(default)]
    pub classes: Vec<CoreClass>,
    /// cluster index per logical core (ARM SoCs group cores in clusters); empty when unknown
    #[serde(default)]
    pub clusters: Vec<u8>,
    #[serde(default)]
    pub sockets: usize,
    pub mem_total: u64,
    #[serde(default)]
    pub swap_total: u64,
    #[serde(default)]
    pub disks: Vec<DiskInfo>,
    #[serde(default)]
    pub ifaces: Vec<IfaceInfo>,
    /// modules the agent fills: "cpu", "mem", "net", "disk", "gpu", "battery", "temp"
    #[serde(default)]
    pub has: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CpuSample {
    /// 0..1
    pub total: f32,
    #[serde(default)]
    pub sys: f32,
    #[serde(default)]
    pub user: f32,
    /// per logical core, 0..1
    #[serde(default)]
    pub cores: Vec<f32>,
    /// current clock per logical core, MHz; empty when the agent cannot read it
    #[serde(default)]
    pub freq: Vec<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct MemSample {
    pub used: u64,
    pub avail: u64,
    #[serde(default)]
    pub swap_used: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NetSample {
    pub id: String,
    /// bytes per second
    pub rx: f64,
    pub tx: f64,
    #[serde(default)]
    pub up: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DiskSample {
    pub id: String,
    /// bytes per second
    pub r: f64,
    pub w: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GpuSample {
    /// 0..1
    pub load: f32,
    #[serde(default)]
    pub vram_used: u64,
    #[serde(default)]
    pub vram_total: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BatterySample {
    /// 0..1
    pub level: f32,
    pub charging: bool,
    pub on_ac: bool,
    #[serde(default)]
    pub remaining_secs: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TempSample {
    pub id: String,
    /// degrees Celsius
    pub c: f32,
}

/// One second of the machine; only rates and fractions, the master keeps the history.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// grows by one per sample so a lost frame is told from a pause
    pub seq: u64,
    pub cpu: CpuSample,
    pub mem: MemSample,
    #[serde(default)]
    pub net: Vec<NetSample>,
    #[serde(default)]
    pub disk: Vec<DiskSample>,
    #[serde(default)]
    pub gpu: Option<GpuSample>,
    #[serde(default)]
    pub battery: Option<BatterySample>,
    #[serde(default)]
    pub temp: Vec<TempSample>,
    /// 1, 5, 15 minute load averages where the OS has them
    #[serde(default)]
    pub load: Vec<f32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub name: String,
    /// 0..1 of the whole machine
    #[serde(default)]
    pub cpu: f32,
    /// resident bytes
    #[serde(default)]
    pub mem: u64,
    /// bytes per second read + written
    #[serde(default)]
    pub io: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DriveSample {
    /// mount point or letter
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub fs: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub readonly: bool,
    pub total: u64,
    pub free: u64,
}

/// Popup material; requested by the master while a popup is open.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Details {
    /// how many rows the master wants
    #[serde(default)]
    pub n: usize,
    #[serde(default)]
    pub top_cpu: Vec<ProcSample>,
    #[serde(default)]
    pub top_mem: Vec<ProcSample>,
    #[serde(default)]
    pub top_io: Vec<ProcSample>,
    #[serde(default)]
    pub drives: Vec<DriveSample>,
    /// IPv4 per interface id
    #[serde(default)]
    pub ips: Vec<(String, String)>,
    /// Wi-Fi network per interface id
    #[serde(default)]
    pub ssids: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    Hello {
        v: u32,
        client: String,
        #[serde(default)]
        token: String,
    },
    Info(Info),
    Snapshot(Snapshot),
    /// from the master: a request (only `n` matters); from the agent: the answer
    Details(Details),
    Ping,
    Pong,
    /// the agent refuses the session (bad token, unsupported version)
    Bye {
        reason: String,
    },
}

/// The socket a master listens on: bound to the beacon port, joined to the multicast group on
/// the default interface and on every address in `ifaces` (beacons from a Wi-Fi segment do not
/// arrive through the Ethernet membership).
pub fn open_beacon_socket(ifaces: &[Ipv4Addr]) -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind(("0.0.0.0", BEACON_PORT))?;
    let group: Ipv4Addr = MULTICAST_ADDR.parse().unwrap();
    let _ = sock.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED);
    for ip in ifaces {
        let _ = sock.join_multicast_v4(&group, ip);
    }
    Ok(sock)
}

/// Length-prefixed JSON frame.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let body = serde_json::to_vec(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        let frames = vec![
            Frame::Hello { v: VERSION, client: "test".into(), token: String::new() },
            Frame::Snapshot(Snapshot { seq: 7, cpu: CpuSample { total: 0.5, cores: vec![0.1, 0.9], ..Default::default() }, ..Default::default() }),
            Frame::Ping,
            Frame::Bye { reason: "no".into() },
        ];
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        for f in &frames {
            assert_eq!(&read_frame(&mut cur).unwrap(), f);
        }
    }

    #[test]
    fn tag_is_t() {
        let s = serde_json::to_string(&Frame::Ping).unwrap();
        assert_eq!(s, r#"{"t":"ping"}"#);
        let f: Frame = serde_json::from_str(r#"{"t":"snapshot","seq":1,"cpu":{"total":0.2},"mem":{"used":1,"avail":2},"extra":true}"#).unwrap();
        assert!(matches!(f, Frame::Snapshot(_)));
    }
}
