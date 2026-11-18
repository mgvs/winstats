//! GPU load where the GPU Engine counters are missing (Windows 7 to 10 1703): the running time
//! of each engine (node) from D3DKMTQueryStatistics, the way Process Explorer reads it. The
//! load of an adapter is its busiest node; a process's load is its busiest node on any adapter.
//! Undocumented, but present in gdi32 since Windows 7 and needs no elevation; processes that
//! cannot be opened (services, other users) are left out.

use std::collections::HashMap;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};
use windows::core::{s, w};

/// D3DKMT_QUERYSTATISTICS_TYPE values
const QUERY_ADAPTER: u32 = 0;
const QUERY_NODE: u32 = 5;
const QUERY_PROCESS_NODE: u32 = 6;

/// D3DKMT_QUERYSTATISTICS_RESULT: the union is this large on every architecture
const RESULT_SIZE: usize = 0x308;
/// Type, AdapterLuid, then the process handle (pointer aligned) and the result (8 aligned)
const HANDLE_AT: usize = if cfg!(target_pointer_width = "64") { 16 } else { 12 };
const RESULT_AT: usize = if cfg!(target_pointer_width = "64") { 24 } else { 16 };
/// the query union after the result: the node id
const NODE_AT: usize = RESULT_AT + RESULT_SIZE;

type QueryFn = unsafe extern "system" fn(*mut u8) -> i32;

pub struct GpuStats {
    query: QueryFn,
    /// (LUID, node count) of the adapters that answer
    adapters: Vec<(u64, u32)>,
    /// running time per (LUID, node) at the last read, 100 ns units
    nodes: HashMap<(u64, u32), i64>,
    nodes_at: Option<Instant>,
    /// running time per (pid, LUID, node) at the last process read
    procs: HashMap<(u32, u64, u32), i64>,
    procs_at: Option<Instant>,
}

impl GpuStats {
    /// None when gdi32 has no D3DKMTQueryStatistics or no adapter reports any node.
    pub fn new(luids: &[u64]) -> Option<Self> {
        let query: QueryFn = unsafe {
            let gdi = LoadLibraryW(w!("gdi32.dll")).ok()?;
            std::mem::transmute(GetProcAddress(gdi, s!("D3DKMTQueryStatistics"))?)
        };
        let mut s = Self { query, adapters: Vec::new(), nodes: HashMap::new(), nodes_at: None, procs: HashMap::new(), procs_at: None };
        for &luid in luids {
            // D3DKMT_QUERYSTATISTICS_ADAPTER_INFORMATION: NbSegments, NodeCount
            if let Some(b) = s.run(QUERY_ADAPTER, luid, HANDLE::default(), 0) {
                let nodes = u32::from_ne_bytes(b[RESULT_AT + 4..RESULT_AT + 8].try_into().unwrap());
                if nodes > 0 && nodes < 256 {
                    s.adapters.push((luid, nodes));
                }
            }
        }
        if s.adapters.is_empty() {
            return None;
        }
        Some(s)
    }

    /// One query; the whole structure on success.
    fn run(&self, kind: u32, luid: u64, process: HANDLE, node: u32) -> Option<[u8; 0x400]> {
        let mut b = [0u8; 0x400];
        b[0..4].copy_from_slice(&kind.to_ne_bytes());
        b[4..8].copy_from_slice(&(luid as u32).to_ne_bytes());
        b[8..12].copy_from_slice(&((luid >> 32) as u32).to_ne_bytes());
        let h = process.0 as usize;
        b[HANDLE_AT..HANDLE_AT + std::mem::size_of::<usize>()].copy_from_slice(&h.to_ne_bytes());
        b[NODE_AT..NODE_AT + 4].copy_from_slice(&node.to_ne_bytes());
        (unsafe { (self.query)(b.as_mut_ptr()) } == 0).then_some(b)
    }

    /// RunningTime, the first field of the node / process node result.
    fn running(b: &[u8; 0x400]) -> i64 {
        i64::from_ne_bytes(b[RESULT_AT..RESULT_AT + 8].try_into().unwrap())
    }

    /// Load per adapter LUID, 0..1; empty on the first call (two reads make a rate).
    pub fn loads(&mut self) -> HashMap<u64, f32> {
        let now = Instant::now();
        let dt = self.nodes_at.map(|t| now.duration_since(t).as_secs_f64() * 1e7).filter(|dt| *dt > 0.0);
        self.nodes_at = Some(now);
        let mut out = HashMap::new();
        for &(luid, count) in &self.adapters.clone() {
            let mut best: Option<f32> = None;
            for node in 0..count {
                let Some(b) = self.run(QUERY_NODE, luid, HANDLE::default(), node) else { continue };
                let t = Self::running(&b);
                if let (Some(old), Some(dt)) = (self.nodes.insert((luid, node), t), dt) {
                    let v = ((t - old).max(0) as f64 / dt).clamp(0.0, 1.0) as f32;
                    best = Some(best.map_or(v, |b| b.max(v)));
                }
            }
            if let Some(v) = best {
                out.insert(luid, v);
            }
        }
        out
    }

    /// Load per (process id, adapter LUID), 0..1: the busiest node of that process on that
    /// adapter, for the processes that can be opened; empty on the first call. GPU popup only: a
    /// query per process and node.
    pub fn by_pid(&mut self, pids: impl Iterator<Item = u32>) -> HashMap<(u32, u64), f32> {
        let now = Instant::now();
        let dt = self.procs_at.map(|t| now.duration_since(t).as_secs_f64() * 1e7).filter(|dt| *dt > 0.0);
        self.procs_at = Some(now);
        let mut seen: HashMap<(u32, u64, u32), i64> = HashMap::new();
        let mut out = HashMap::new();
        for pid in pids {
            if pid <= 4 {
                continue;
            }
            let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) }) else { continue };
            for &(luid, count) in &self.adapters {
                let mut best: Option<f32> = None;
                for node in 0..count {
                    // a process that never used this adapter fails on its first node already
                    let Some(b) = self.run(QUERY_PROCESS_NODE, luid, h, node) else { break };
                    let t = Self::running(&b);
                    seen.insert((pid, luid, node), t);
                    if let (Some(old), Some(dt)) = (self.procs.get(&(pid, luid, node)), dt) {
                        let v = ((t - old).max(0) as f64 / dt).clamp(0.0, 1.0) as f32;
                        best = Some(best.map_or(v, |b| b.max(v)));
                    }
                }
                if let Some(v) = best {
                    out.insert((pid, luid), v);
                }
            }
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        // exited processes drop out
        self.procs = seen;
        out
    }
}

#[derive(Default)]
struct Shared {
    loads: HashMap<u64, f32>,
    /// processes to read (GPU popup open); empty: adapters only
    pids: Vec<u32>,
    by_pid: HashMap<(u32, u64), f32>,
}

/// The queries take tens to hundreds of ms (a sleeping Optimus GPU answers slowly): they run on
/// their own thread once a second, the UI takes the last results.
pub struct GpuStatsWorker {
    st: std::sync::Arc<std::sync::Mutex<Shared>>,
}

impl GpuStatsWorker {
    pub fn start(mut stats: GpuStats) -> Self {
        let st = std::sync::Arc::new(std::sync::Mutex::new(Shared::default()));
        let shared = st.clone();
        let _ = std::thread::Builder::new().name("gpu-load".into()).spawn(move || loop {
            let loads = stats.loads();
            let pids = {
                let mut s = shared.lock().unwrap();
                s.loads = loads;
                s.pids.clone()
            };
            if pids.is_empty() {
                stats.procs.clear();
                stats.procs_at = None;
            } else {
                let by_pid = stats.by_pid(pids.into_iter());
                shared.lock().unwrap().by_pid = by_pid;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        });
        Self { st }
    }

    /// Load per adapter LUID from the last read.
    pub fn loads(&self) -> HashMap<u64, f32> {
        self.st.lock().unwrap().loads.clone()
    }

    /// Ask for the processes too (empty: stop) and take the last per-process loads.
    pub fn by_pid(&self, pids: Vec<u32>) -> HashMap<(u32, u64), f32> {
        let mut s = self.st.lock().unwrap();
        if pids.is_empty() {
            s.by_pid.clear();
        }
        s.pids = pids;
        s.by_pid.clone()
    }
}
