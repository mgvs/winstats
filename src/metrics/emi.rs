//! Energy Meter Interface (EMI): energy counters some platforms expose to user mode
//! (Intel RAPL through the Power Engine Plug-in, Qualcomm and Surface rails). Each channel is an
//! accumulating energy counter in picowatt-hours with a 100 ns timestamp; power is the
//! difference between two reads.
//!
//! Only channels whose name says what they meter are classified: a platform / PSYS rail is the
//! whole machine's draw, a PKG rail the CPU package. Nothing else is guessed. Machines without
//! an EMI device, or whose device denies access, simply have no meters.

use super::power::{interface_paths, ioctl, ioctl_raw, open_device, MeterChannel, MeterRole};
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE};

/// {45BD8344-7ED6-49cf-A440-C276C933B053}
const GUID_DEVICE_ENERGY_METER: GUID = GUID::from_u128(0x45bd8344_7ed6_49cf_a440_c276c933b053);

// CTL_CODE(FILE_DEVICE_UNKNOWN = 0x22, fn, METHOD_BUFFERED, FILE_READ_ACCESS)
const IOCTL_EMI_GET_VERSION: u32 = 0x0022_4000;
const IOCTL_EMI_GET_METADATA_SIZE: u32 = 0x0022_4004;
const IOCTL_EMI_GET_METADATA: u32 = 0x0022_4008;
const IOCTL_EMI_GET_MEASUREMENT: u32 = 0x0022_400C;

const EMI_VERSION_V1: u16 = 1;
const EMI_VERSION_V2: u16 = 2;
/// EmiMeasurementUnitPicowattHours
const UNIT_PWH: i32 = 0;

struct Device {
    handle: HANDLE,
    version: u16,
    /// channel names in measurement order; None = a unit we do not understand (skipped)
    channels: Vec<Option<String>>,
    /// previous (energy pWh, time 100 ns) per channel
    prev: Vec<Option<(u64, u64)>>,
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

pub struct Meters {
    devices: Vec<Device>,
    failures: u32,
}

fn utf16_at(buf: &[u8], off: usize, bytes: usize) -> String {
    let end = (off + bytes).min(buf.len());
    if off >= end {
        return String::new();
    }
    let w: Vec<u16> = buf[off..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
    String::from_utf16_lossy(&w).trim().to_string()
}

impl Device {
    fn open(path: &str) -> Option<Self> {
        let handle = open_device(path, false)?;
        let close = |h: HANDLE| unsafe {
            let _ = CloseHandle(h);
        };
        let mut version = 0u16;
        if ioctl::<(), u16>(handle, IOCTL_EMI_GET_VERSION, None, &mut version).is_none() {
            close(handle);
            return None;
        }
        let mut size = 0u32;
        if ioctl::<(), u32>(handle, IOCTL_EMI_GET_METADATA_SIZE, None, &mut size).is_none() || size == 0 || size > 1 << 16 {
            close(handle);
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let Some(got) = ioctl_raw::<()>(handle, IOCTL_EMI_GET_METADATA, None, buf.as_mut_ptr(), buf.len()) else {
            close(handle);
            return None;
        };
        buf.truncate(got as usize);
        let channels = match version {
            // EMI_METADATA_V1: unit (4), OEM [16] WCHAR, model [16] WCHAR, revision (2),
            // name size (2), name
            EMI_VERSION_V1 if buf.len() >= 72 => {
                let unit = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let name_size = u16::from_le_bytes([buf[70], buf[71]]) as usize;
                vec![(unit == UNIT_PWH).then(|| utf16_at(&buf, 72, name_size))]
            }
            // EMI_METADATA_V2: OEM [16] WCHAR, model [16] WCHAR, revision (2), channel count (2),
            // then EMI_CHANNEL_V2 { unit (4), name size (2), name } back to back
            EMI_VERSION_V2 if buf.len() >= 68 => {
                let count = u16::from_le_bytes([buf[66], buf[67]]) as usize;
                let mut off = 68;
                let mut out = Vec::with_capacity(count);
                for _ in 0..count {
                    if off + 6 > buf.len() {
                        break;
                    }
                    let unit = i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
                    let name_size = u16::from_le_bytes([buf[off + 4], buf[off + 5]]) as usize;
                    out.push((unit == UNIT_PWH).then(|| utf16_at(&buf, off + 6, name_size)));
                    off += 6 + name_size;
                }
                out
            }
            _ => Vec::new(),
        };
        if channels.iter().all(|c| c.is_none()) {
            close(handle);
            return None;
        }
        let prev = vec![None; channels.len()];
        Some(Device { handle, version, channels, prev })
    }

    /// Average power per channel since the previous read; empty on the first read.
    fn sample(&mut self) -> Option<Vec<MeterChannel>> {
        let n = if self.version == EMI_VERSION_V1 { 1 } else { self.channels.len() };
        let mut buf = vec![0u64; n * 2];
        let got = ioctl_raw::<()>(self.handle, IOCTL_EMI_GET_MEASUREMENT, None, buf.as_mut_ptr() as *mut u8, buf.len() * 8)?;
        let n = n.min(got as usize / 16);
        let mut out = Vec::new();
        for i in 0..n {
            let (e, t) = (buf[i * 2], buf[i * 2 + 1]);
            let Some(name) = self.channels[i].clone() else { continue };
            if let Some((pe, pt)) = self.prev[i] {
                if t > pt && e >= pe {
                    // pWh -> J: * 3600e-12; 100 ns -> s: * 1e-7
                    let joules = (e - pe) as f64 * 3.6e-9;
                    let secs = (t - pt) as f64 * 1e-7;
                    let watts = (joules / secs) as f32;
                    if watts.is_finite() && (0.0..2000.0).contains(&watts) {
                        out.push(MeterChannel { role: classify(&name), name, watts });
                    }
                }
            }
            self.prev[i] = Some((e, t));
        }
        Some(out)
    }
}

/// What a channel meters, from its name only.
pub fn classify(name: &str) -> MeterRole {
    let n = name.to_ascii_uppercase();
    if n.contains("PSYS") || n.contains("PLATFORM") || n.ends_with("_SYS") || n == "SYS" || n.contains("SYSTEM") {
        MeterRole::Platform
    } else if n.contains("PKG") || n.contains("PACKAGE_ENERGY") {
        MeterRole::CpuPackage
    } else {
        MeterRole::Other
    }
}

impl Meters {
    /// Readable energy meters on this machine (opened and closed again).
    pub fn available() -> bool {
        Self::open().is_some()
    }

    /// None when no energy meter can be read on this machine.
    pub fn open() -> Option<Self> {
        let devices: Vec<Device> = interface_paths(&GUID_DEVICE_ENERGY_METER).iter().filter_map(|p| Device::open(p)).collect();
        if devices.is_empty() {
            None
        } else {
            Some(Self { devices, failures: 0 })
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.devices.iter().flat_map(|d| d.channels.iter().flatten().cloned()).collect()
    }

    pub fn sample(&mut self) -> Vec<MeterChannel> {
        let mut out = Vec::new();
        let mut ok = false;
        for d in self.devices.iter_mut() {
            if let Some(v) = d.sample() {
                ok = true;
                out.extend(v);
            }
        }
        self.failures = if ok { 0 } else { self.failures + 1 };
        out
    }

    /// Reads keep failing: give up on the meters.
    pub fn failed(&self) -> bool {
        self.failures > 5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_roles_come_from_the_name_only() {
        assert_eq!(classify("RAPL_Package0_PSYS"), MeterRole::Platform);
        assert_eq!(classify("RAPL_Package0_PKG"), MeterRole::CpuPackage);
        assert_eq!(classify("RAPL_Package0_PP0"), MeterRole::Other);
        assert_eq!(classify("RAPL_Package0_DRAM"), MeterRole::Other);
    }
}
