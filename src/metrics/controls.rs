//! The battery popup's slow reads on a thread of their own: the power settings and the GPUs'
//! power states. Device queries can take tens of milliseconds on some machines; on the UI
//! thread they would stall the power-flow animation once per tick.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{gpu, quick};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub screen_off: Option<(u32, u32)>,
    /// (vendor, device, D-state) of the display adapters
    pub gpu_states: Vec<(u32, u32, u8)>,
}

/// One full read, on whatever thread calls it.
pub fn read() -> Snapshot {
    Snapshot {
        screen_off: quick::screen_timeouts(),
        gpu_states: gpu::power_states(),
    }
}

#[derive(Default)]
struct Shared {
    active: bool,
    every: Duration,
    /// the newest read since the popup opened (None right after it opened)
    snap: Option<Snapshot>,
}

pub struct ControlsWorker {
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

impl ControlsWorker {
    pub fn start() -> Self {
        let shared = Arc::new((Mutex::new(Shared { every: Duration::from_millis(1000), ..Default::default() }), Condvar::new()));
        let s = shared.clone();
        std::thread::spawn(move || {
            let (m, cv) = &*s;
            loop {
                let mut g = m.lock().unwrap();
                while !g.active {
                    g = cv.wait(g).unwrap();
                }
                drop(g);
                let snap = read();
                let mut g = m.lock().unwrap();
                if g.active {
                    g.snap = Some(snap);
                }
                let every = g.every;
                let _ = cv.wait_timeout(g, every).unwrap();
            }
        });
        Self { shared }
    }

    /// Popup open (reads every `ms`) or closed (the thread sleeps).
    pub fn set_active(&self, active: bool, ms: u32) {
        let (m, cv) = &*self.shared;
        let mut g = m.lock().unwrap();
        g.every = Duration::from_millis(ms.max(500) as u64);
        if g.active != active {
            g.active = active;
            if !active {
                g.snap = None;
            }
            cv.notify_all();
        }
    }

    /// The newest read since the popup opened.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.shared.0.lock().unwrap().snap.clone()
    }

    /// A read done elsewhere (on opening, after a setting changed) counts as the newest.
    pub fn put(&self, snap: Snapshot) {
        let mut g = self.shared.0.lock().unwrap();
        if g.active {
            g.snap = Some(snap);
        }
    }
}
