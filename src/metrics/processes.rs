//! Per-process CPU and memory through NtQuerySystemInformation(SystemProcessInformation).
//! Only sampled while the popup is open.

use std::collections::HashMap;
use std::time::Instant;
use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

/// Leading part of SYSTEM_PROCESS_INFORMATION (x64 layout); we never read past `working_set_size`.
#[repr(C)]
struct SystemProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    /// 0..1 of the whole machine
    pub cpu: f32,
    /// private working set, bytes
    pub mem: u64,
    /// read + write transfer, bytes/s (all I/O the process does: files, pipes, sockets)
    pub io: f64,
    /// GPU process list only: busiest engine 0..1 and dedicated + shared GPU memory, bytes
    pub gpu: f32,
    pub vram: u64,
    /// run by the system (services, session 0, other accounts such as DWM) rather than the user
    pub system: bool,
}

pub struct Processes {
    /// (pid, create time) -> (cpu time, read+write transfer bytes)
    prev: HashMap<(u32, i64), (i64, i64)>,
    prev_at: Option<Instant>,
    buf: Vec<u8>,
    cores: usize,
    /// rows per list
    pub top_n: usize,
    pub top_cpu: Vec<ProcInfo>,
    pub top_mem: Vec<ProcInfo>,
    pub top_io: Vec<ProcInfo>,
    /// sum of every process's private working set, bytes
    pub mem_total: u64,
    /// image name and system flag per process id from the last sample (the GPU list)
    pub names: HashMap<u32, (String, bool)>,
    /// which lists show system and user processes (the SYS / USER filter buttons)
    pub show_system: bool,
    pub show_user: bool,
    /// system flag per (pid, create time): the owner check opens the process, so once only
    owners: HashMap<(u32, i64), bool>,
    /// SID of the account winstats runs as
    my_sid: Vec<u8>,
}

impl Processes {
    pub fn new(cores: usize, top_n: usize) -> Self {
        Self {
            prev: HashMap::new(),
            prev_at: None,
            buf: vec![0; 512 * 1024],
            cores: cores.max(1),
            top_n: top_n.max(1),
            top_cpu: Vec::new(),
            top_mem: Vec::new(),
            top_io: Vec::new(),
            mem_total: 0,
            names: HashMap::new(),
            show_system: true,
            show_user: true,
            owners: HashMap::new(),
            my_sid: current_sid().unwrap_or_default(),
        }
    }

    pub fn reset(&mut self) {
        self.prev.clear();
        self.prev_at = None;
    }

    fn query(&mut self) -> bool {
        for _ in 0..4 {
            let mut ret = 0u32;
            let status = unsafe {
                NtQuerySystemInformation(SystemProcessInformation, self.buf.as_mut_ptr() as *mut _, self.buf.len() as u32, &mut ret)
            };
            if status.is_ok() {
                return true;
            }
            // STATUS_INFO_LENGTH_MISMATCH: grow and retry
            let want = (ret as usize).max(self.buf.len() * 2);
            self.buf.resize(want + 64 * 1024, 0);
        }
        false
    }

    pub fn update(&mut self) {
        if !self.query() {
            return;
        }
        let now = Instant::now();
        let dt = self.prev_at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        let mut cur: HashMap<(u32, i64), (i64, i64)> = HashMap::new();
        let mut all: Vec<ProcInfo> = Vec::new();
        let mut off = 0usize;
        loop {
            if off + std::mem::size_of::<SystemProcessInformation>() > self.buf.len() {
                break;
            }
            let p = unsafe { &*(self.buf.as_ptr().add(off) as *const SystemProcessInformation) };
            let pid = p.unique_process_id as u32;
            let name = if p.image_name.buffer.is_null() || p.image_name.length == 0 {
                if pid == 0 { "System Idle".to_string() } else { "System".to_string() }
            } else {
                let s = unsafe { std::slice::from_raw_parts(p.image_name.buffer, (p.image_name.length / 2) as usize) };
                String::from_utf16_lossy(s)
            };
            let t = p.kernel_time + p.user_time;
            let xfer = p.read_transfer_count.saturating_add(p.write_transfer_count);
            let key = (pid, p.create_time);
            let (cpu, io) = match (self.prev.get(&key), dt > 0.0) {
                (Some((pt, px)), true) => (
                    ((t - pt) as f64 / 1e7 / dt / self.cores as f64).clamp(0.0, 1.0) as f32,
                    ((xfer - px).max(0) as f64 / dt),
                ),
                _ => (0.0, 0.0),
            };
            cur.insert(key, (t, xfer));
            if pid != 0 {
                let system = match self.owners.get(&key) {
                    Some(v) => *v,
                    None => {
                        let v = is_system(pid, p.session_id, &self.my_sid);
                        self.owners.insert(key, v);
                        v
                    }
                };
                all.push(ProcInfo { pid, name, cpu, mem: p.working_set_private_size.max(0) as u64, io, gpu: 0.0, vram: 0, system });
            }
            if p.next_entry_offset == 0 {
                break;
            }
            off += p.next_entry_offset as usize;
        }
        // forget the owners of processes that are gone
        self.owners.retain(|k, _| cur.contains_key(k));
        self.prev = cur;
        self.prev_at = Some(now);

        self.mem_total = all.iter().map(|p| p.mem).sum();
        self.names = all.iter().map(|p| (p.pid, (p.name.clone(), p.system))).collect();
        let (show_system, show_user) = (self.show_system, self.show_user);
        all.retain(|p| if p.system { show_system } else { show_user });
        // merge same-name processes (browser tabs etc.) into one row per app
        let mut merged: HashMap<String, ProcInfo> = HashMap::new();
        for p in all {
            let e = merged.entry(p.name.clone()).or_insert(ProcInfo { pid: p.pid, name: p.name.clone(), cpu: 0.0, mem: 0, io: 0.0, gpu: 0.0, vram: 0, system: p.system });
            e.cpu += p.cpu;
            e.mem += p.mem;
            e.io += p.io;
        }
        let n = self.top_n;
        let mut v: Vec<ProcInfo> = merged.into_values().collect();
        v.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
        self.top_cpu = v.iter().take(n).cloned().collect();
        v.sort_by(|a, b| b.mem.cmp(&a.mem));
        self.top_mem = v.iter().take(n).cloned().collect();
        v.sort_by(|a, b| b.io.partial_cmp(&a.io).unwrap_or(std::cmp::Ordering::Equal));
        self.top_io = v.iter().filter(|p| p.io > 0.0).take(n).cloned().collect();
    }
}

/// The GPU list of one adapter (`luid`; None: all of them): per-process load (busiest engine)
/// and GPU memory on it, merged by image name like the other lists, busiest first (or the most
/// video memory first, `by_vram`); processes without GPU work or memory there are left out.
pub fn top_gpu(load: &HashMap<(u32, u64), f32>, mem: &HashMap<(u32, u64), (u64, u64)>, names: &HashMap<u32, (String, bool)>, filter: (bool, bool), n: usize, by_vram: bool, luid: Option<u64>) -> Vec<ProcInfo> {
    let mine = |l: u64| luid.map_or(true, |want| want == l);
    let mut per_pid: HashMap<u32, (f32, u64)> = HashMap::new();
    for (&(pid, l), &g) in load {
        if mine(l) {
            let e = per_pid.entry(pid).or_default();
            e.0 = e.0.max(g);
        }
    }
    for (&(pid, l), m) in mem {
        if mine(l) {
            per_pid.entry(pid).or_default().1 += m.0 + m.1;
        }
    }
    let mut merged: HashMap<String, ProcInfo> = HashMap::new();
    for (pid, (g, v)) in per_pid {
        if g <= 0.0 && v == 0 {
            continue;
        }
        // a process that is not in the last snapshot counts as a system one
        let (name, system) = names.get(&pid).cloned().unwrap_or_else(|| (format!("pid {pid}"), true));
        if !(if system { filter.0 } else { filter.1 }) {
            continue;
        }
        let e = merged.entry(name.clone()).or_insert(ProcInfo { pid, name, cpu: 0.0, mem: 0, io: 0.0, gpu: 0.0, vram: 0, system });
        // engines are shared: the app's load is its busiest process, memory adds up
        e.gpu = e.gpu.max(g);
        e.vram += v;
    }
    let mut v: Vec<ProcInfo> = merged.into_values().collect();
    sort_gpu(&mut v, by_vram);
    v.truncate(n);
    v
}

/// GPU list order: the busiest first, or the most video memory first; the other one breaks ties.
pub fn sort_gpu(v: &mut [ProcInfo], by_vram: bool) {
    let load = |a: &ProcInfo, b: &ProcInfo| b.gpu.partial_cmp(&a.gpu).unwrap_or(std::cmp::Ordering::Equal);
    if by_vram {
        v.sort_by(|a, b| b.vram.cmp(&a.vram).then(load(a, b)));
    } else {
        v.sort_by(|a, b| load(a, b).then(b.vram.cmp(&a.vram)));
    }
}

/// The account (SID bytes) of this process.
fn current_sid() -> Option<Vec<u8>> {
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe { token_sid(GetCurrentProcess()) }
}

/// SID bytes of the account a process runs as.
unsafe fn token_sid(process: windows::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::OpenProcessToken;
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    let mut buf = vec![0u8; 256];
    let mut len = 0u32;
    let ok = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, &mut len).is_ok();
    let _ = CloseHandle(token);
    if !ok {
        return None;
    }
    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let sid = user.User.Sid;
    if sid.0.is_null() {
        return None;
    }
    let n = GetLengthSid(sid) as usize;
    Some(std::slice::from_raw_parts(sid.0 as *const u8, n).to_vec())
}

/// System process: session 0 (services, the kernel), or another account than ours (DWM, SYSTEM,
/// other users). One the user cannot even open is the system's too.
fn is_system(pid: u32, session: u32, mine: &[u8]) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid <= 4 || session == 0 || mine.is_empty() {
        return true;
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return true };
        let sid = token_sid(h);
        let _ = CloseHandle(h);
        sid.map_or(true, |s| s != mine)
    }
}

/// Terminate `pid` (the kill button in the popup). False when Windows refuses: system processes
/// and processes of other users need elevation.
pub fn kill(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else { return false };
        let ok = TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}
