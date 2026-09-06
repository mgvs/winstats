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
                all.push(ProcInfo { pid, name, cpu, mem: p.working_set_private_size.max(0) as u64, io });
            }
            if p.next_entry_offset == 0 {
                break;
            }
            off += p.next_entry_offset as usize;
        }
        self.prev = cur;
        self.prev_at = Some(now);

        self.mem_total = all.iter().map(|p| p.mem).sum();
        // merge same-name processes (browser tabs etc.) into one row per app
        let mut merged: HashMap<String, ProcInfo> = HashMap::new();
        for p in all {
            let e = merged.entry(p.name.clone()).or_insert(ProcInfo { pid: p.pid, name: p.name.clone(), cpu: 0.0, mem: 0, io: 0.0 });
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
