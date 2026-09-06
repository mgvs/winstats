//! Per-core CPU times on Windows straight from ntdll, the way winstats itself reads them.
//! sysinfo goes through PDH counters, which come back empty on non-English Windows.

use std::mem::size_of;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PerfInfo {
    idle: i64,
    kernel: i64,
    user: i64,
    dpc: i64,
    interrupt: i64,
    interrupt_count: u32,
}

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;

#[link(name = "ntdll")]
extern "system" {
    fn NtQuerySystemInformation(class: u32, info: *mut core::ffi::c_void, len: u32, ret_len: *mut u32) -> i32;
}

pub struct WinCpu {
    prev: Vec<PerfInfo>,
    /// (per core 0..1, total, sys, user)
    pub cores: Vec<f32>,
    pub total: f32,
    pub sys: f32,
    pub user: f32,
}

impl WinCpu {
    pub fn new(n: usize) -> Self {
        let mut c = Self { prev: Vec::new(), cores: vec![0.0; n], total: 0.0, sys: 0.0, user: 0.0 };
        c.prev = c.read();
        c
    }

    fn read(&self) -> Vec<PerfInfo> {
        let mut buf = vec![PerfInfo::default(); 256];
        let mut ret = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                buf.as_mut_ptr() as *mut _,
                (buf.len() * size_of::<PerfInfo>()) as u32,
                &mut ret,
            )
        };
        if status < 0 {
            return Vec::new();
        }
        buf.truncate(ret as usize / size_of::<PerfInfo>());
        buf
    }

    pub fn update(&mut self) {
        let cur = self.read();
        if cur.len() != self.prev.len() || cur.is_empty() {
            self.prev = cur;
            return;
        }
        let (mut busy_all, mut sys_all, mut user_all, mut span_all) = (0f64, 0f64, 0f64, 0f64);
        self.cores.resize(cur.len(), 0.0);
        for (i, (a, b)) in self.prev.iter().zip(cur.iter()).enumerate() {
            // kernel time includes idle time
            let idle = (b.idle - a.idle).max(0) as f64;
            let kernel = (b.kernel - a.kernel).max(0) as f64;
            let user = (b.user - a.user).max(0) as f64;
            let span = kernel + user;
            let busy = (span - idle).max(0.0);
            self.cores[i] = if span > 0.0 { (busy / span) as f32 } else { 0.0 };
            busy_all += busy;
            sys_all += (kernel - idle).max(0.0);
            user_all += user;
            span_all += span;
        }
        if span_all > 0.0 {
            self.total = (busy_all / span_all) as f32;
            self.sys = (sys_all / span_all) as f32;
            self.user = (user_all / span_all) as f32;
        }
        self.prev = cur;
    }
}

