//! Per-core CPU usage through NtQuerySystemInformation(SystemProcessorPerformanceInformation),
//! the same source the classic Task Manager uses.

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessorPerformanceInformation};
use windows::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, GetSystemInfo, LOGICAL_PROCESSOR_RELATIONSHIP, RelationProcessorCore, RelationProcessorPackage, SYSTEM_INFO};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcPerf {
    idle: i64,
    kernel: i64,
    user: i64,
    dpc: i64,
    interrupt: i64,
    interrupt_count: u32,
}

pub struct Cpu {
    prev: Vec<ProcPerf>,
    pub cores: Vec<f32>,
    pub total: f32,
    pub system: f32,
    pub user: f32,
    pub name: String,
    /// EfficiencyClass per logical processor (higher = faster core; hybrid Intel: P = 1, E = 0).
    pub classes: Vec<u8>,
    /// Package (socket) index per logical processor.
    pub packages: Vec<u8>,
}

/// EfficiencyClass per logical CPU via GetLogicalProcessorInformationEx(RelationProcessorCore).
fn efficiency_classes(n: usize) -> Vec<u8> {
    relation_ids(n, RelationProcessorCore, true)
}

/// Package (socket) index per logical CPU via RelationProcessorPackage.
fn package_ids(n: usize) -> Vec<u8> {
    relation_ids(n, RelationProcessorPackage, false)
}

/// Walks GetLogicalProcessorInformationEx for `relation`; per logical CPU stores either the
/// EfficiencyClass byte of its entry (`use_class`) or the running index of the entry.
fn relation_ids(n: usize, relation: LOGICAL_PROCESSOR_RELATIONSHIP, use_class: bool) -> Vec<u8> {
    let mut classes = vec![0u8; n];
    let mut len = 0u32;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(relation, None, &mut len);
        if len == 0 {
            return classes;
        }
        let mut buf = vec![0u8; len as usize];
        if GetLogicalProcessorInformationEx(relation, Some(buf.as_mut_ptr() as *mut _), &mut len).is_err() {
            return classes;
        }
        let mut off = 0usize;
        let mut entry: u8 = 0;
        while off + 32 <= len as usize {
            let size = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
            if size == 0 {
                break;
            }
            // PROCESSOR_RELATIONSHIP at +8: Flags u8, EfficiencyClass u8, Reserved[20], GroupCount u16, GroupMask[]
            let class = if use_class { buf[off + 9] } else { entry };
            entry = entry.wrapping_add(1);
            let group_count = u16::from_le_bytes(buf[off + 30..off + 32].try_into().unwrap()) as usize;
            for g in 0..group_count {
                let ga = off + 32 + g * 16; // GROUP_AFFINITY { Mask: usize, Group: u16, Reserved: [u16; 3] }
                if ga + 16 > len as usize {
                    break;
                }
                let mask = u64::from_le_bytes(buf[ga..ga + 8].try_into().unwrap());
                let group = u16::from_le_bytes(buf[ga + 8..ga + 10].try_into().unwrap());
                if group != 0 {
                    continue; // only the first processor group is sampled
                }
                for bit in 0..64 {
                    if mask & (1u64 << bit) != 0 && bit < n {
                        classes[bit] = class;
                    }
                }
            }
            off += size;
        }
    }
    classes
}

impl Cpu {
    pub fn new() -> Self {
        let n = unsafe {
            let mut si = SYSTEM_INFO::default();
            GetSystemInfo(&mut si);
            si.dwNumberOfProcessors.max(1) as usize
        };
        let name = crate::util::reg_sz_hklm(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString")
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let classes = efficiency_classes(n);
        let packages = package_ids(n);
        let mut c = Self { prev: vec![ProcPerf::default(); n], cores: vec![0.0; n], total: 0.0, system: 0.0, user: 0.0, name, classes, packages };
        let _ = c.query(); // prime
        c
    }

    fn query(&mut self) -> Option<Vec<ProcPerf>> {
        let n = self.prev.len();
        let mut buf = vec![ProcPerf::default(); n];
        let mut ret = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessorPerformanceInformation,
                buf.as_mut_ptr() as *mut _,
                (n * std::mem::size_of::<ProcPerf>()) as u32,
                &mut ret,
            )
        };
        if status.is_err() {
            return None;
        }
        let got = (ret as usize / std::mem::size_of::<ProcPerf>()).min(n);
        buf.truncate(got);
        let out = buf.clone();
        self.prev = buf;
        Some(out)
    }

    pub fn update(&mut self) {
        let prev = self.prev.clone();
        let Some(cur) = self.query() else { return };
        let n = cur.len().min(prev.len());
        self.cores.resize(n, 0.0);
        let mut busy_sum = 0f64;
        let mut sys_sum = 0f64;
        let mut user_sum = 0f64;
        let mut total_sum = 0f64;
        for i in 0..n {
            // kernel time includes idle time
            let kernel = cur[i].kernel - prev[i].kernel;
            let user = cur[i].user - prev[i].user;
            let idle = cur[i].idle - prev[i].idle;
            let total = kernel + user;
            let busy = (total - idle).max(0);
            let u = if total > 0 { busy as f64 / total as f64 } else { 0.0 };
            self.cores[i] = u.clamp(0.0, 1.0) as f32;
            busy_sum += busy as f64;
            sys_sum += (kernel - idle).max(0) as f64;
            user_sum += user.max(0) as f64;
            total_sum += total as f64;
        }
        if total_sum > 0.0 {
            self.total = (busy_sum / total_sum).clamp(0.0, 1.0) as f32;
            self.system = (sys_sum / total_sum).clamp(0.0, 1.0) as f32;
            self.user = (user_sum / total_sum).clamp(0.0, 1.0) as f32;
        }
    }
}
