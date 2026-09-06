//! Memory modules from the SMBIOS table (structure type 17), read with GetSystemFirmwareTable:
//! no WMI, no driver, works from Windows 7 on.

use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

#[derive(Clone, Debug)]
pub struct MemModule {
    pub slot: String,
    pub bank: String,
    pub mem_type: &'static str,
    /// rated speed, MT/s
    pub speed: u32,
    /// configured speed, MT/s (0 when the table does not carry it)
    pub configured_speed: u32,
    pub manufacturer: String,
    pub part: String,
    pub size: u64,
}

fn type_name(t: u8) -> &'static str {
    match t {
        0x03 => "DRAM",
        0x0F => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => "",
    }
}

fn raw_table() -> Option<Vec<u8>> {
    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let got = GetSystemFirmwareTable(RSMB, 0, Some(&mut buf));
        if got == 0 {
            return None;
        }
        buf.truncate(got as usize);
        Some(buf)
    }
}

/// String `index` (1-based) from the string area that follows a formatted structure.
fn smbios_string(strings: &[u8], index: u8) -> String {
    if index == 0 {
        return String::new();
    }
    let mut n = 1u8;
    let mut start = 0usize;
    for (i, b) in strings.iter().enumerate() {
        if *b == 0 {
            if n == index {
                return String::from_utf8_lossy(&strings[start..i]).trim().to_string();
            }
            n += 1;
            start = i + 1;
            if i + 1 < strings.len() && strings[i + 1] == 0 {
                break;
            }
        }
    }
    String::new()
}

pub fn memory_modules() -> Vec<MemModule> {
    let Some(raw) = raw_table() else { return Vec::new() };
    // RawSMBIOSData: Used20CallingMethod, MajorVersion, MinorVersion, DmiRevision, Length (u32), table...
    if raw.len() < 8 {
        return Vec::new();
    }
    let table = &raw[8..];
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= table.len() {
        let stype = table[off];
        let len = table[off + 1] as usize;
        if len < 4 || off + len > table.len() {
            break;
        }
        let formatted = &table[off..off + len];
        // strings: NUL-terminated, the area ends with an extra NUL
        let mut end = off + len;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        let strings = &table[off + len..(end + 2).min(table.len())];
        if stype == 127 {
            break;
        }
        if stype == 17 && len >= 0x15 {
            let u16_at = |o: usize| u16::from_le_bytes([formatted[o], formatted[o + 1]]);
            let raw_size = u16_at(0x0C);
            let mut size = if raw_size == 0xFFFF || raw_size == 0 {
                0
            } else if raw_size & 0x8000 != 0 {
                (raw_size & 0x7FFF) as u64 * 1024
            } else {
                raw_size as u64 * 1024 * 1024
            };
            if raw_size == 0x7FFF && len >= 0x20 {
                let ext = u32::from_le_bytes([formatted[0x1C], formatted[0x1D], formatted[0x1E], formatted[0x1F]]);
                size = (ext & 0x7FFF_FFFF) as u64 * 1024 * 1024;
            }
            if size > 0 {
                out.push(MemModule {
                    slot: smbios_string(strings, formatted[0x10]),
                    bank: smbios_string(strings, formatted[0x11]),
                    mem_type: type_name(formatted[0x12]),
                    speed: if len >= 0x17 { u16_at(0x15) as u32 } else { 0 },
                    configured_speed: if len >= 0x22 { u16_at(0x20) as u32 } else { 0 },
                    manufacturer: if len >= 0x18 { smbios_string(strings, formatted[0x17]) } else { String::new() },
                    part: if len >= 0x1B { smbios_string(strings, formatted[0x1A]) } else { String::new() },
                    size,
                });
            }
        }
        off = end + 2;
    }
    out
}

/// Channel id of a module from its locator strings: "Controller0-ChannelB-DIMM0" -> "0B",
/// "ChannelA-DIMM1" -> "A", "DIMM_B2" -> "B". None when the strings say nothing about it.
fn channel_id(m: &MemModule) -> Option<String> {
    for s in [&m.bank, &m.slot] {
        let lower = s.to_ascii_lowercase();
        if let Some(p) = lower.find("channel") {
            let rest = &lower[p + 7..];
            let rest = rest.trim_start_matches(|c: char| c == ' ' || c == '_' || c == '-' || c == '#');
            let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if id.is_empty() {
                continue;
            }
            let ctrl = lower.find("controller").map(|q| {
                let r = &lower[q + 10..];
                r.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()
            });
            return Some(format!("{}{}", ctrl.unwrap_or_default(), id));
        }
    }
    // "DIMM_A1", "DIMM A2", "DIMM-B1": the letter is the channel
    let lower = m.slot.to_ascii_lowercase();
    if let Some(p) = lower.find("dimm") {
        let rest = lower[p + 4..].trim_start_matches(|c: char| c == ' ' || c == '_' || c == '-');
        let mut it = rest.chars();
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            if a.is_ascii_alphabetic() && b.is_ascii_digit() {
                return Some(a.to_string());
            }
        }
    }
    None
}

/// Number of memory channels in use: distinct channel ids of the populated modules.
pub fn channels(modules: &[MemModule]) -> Option<usize> {
    let mut ids: Vec<String> = Vec::new();
    for m in modules {
        let id = channel_id(m)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        None
    } else {
        Some(ids.len())
    }
}

/// "DDR5 6000 MT/s, 2 x 16 GB" style summary, or None without SMBIOS data.
pub fn summary(modules: &[MemModule]) -> Option<String> {
    let first = modules.first()?;
    let speed = if first.configured_speed > 0 { first.configured_speed } else { first.speed };
    let mut s = String::new();
    if !first.mem_type.is_empty() {
        s.push_str(first.mem_type);
    }
    if speed > 0 {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("{speed} MT/s"));
    }
    let same = modules.iter().all(|m| m.size == first.size);
    let sizes = if same {
        format!("{} \u{00D7} {}", modules.len(), crate::util::format_bytes(first.size))
    } else {
        modules.iter().map(|m| crate::util::format_bytes(m.size)).collect::<Vec<_>>().join(" + ")
    };
    if !s.is_empty() {
        s.push_str(", ");
    }
    s.push_str(&sizes);
    Some(s)
}
