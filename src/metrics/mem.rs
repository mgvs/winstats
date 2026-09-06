use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Default, Clone, Copy)]
pub struct Mem {
    pub total: u64,
    pub used: u64,
    pub pct: f32,
    /// commit charge (physical + page file)
    pub commit_used: u64,
    pub commit_total: u64,
}

impl Mem {
    pub fn update(&mut self) {
        let mut st = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if unsafe { GlobalMemoryStatusEx(&mut st) }.is_ok() {
            self.total = st.ullTotalPhys;
            self.used = st.ullTotalPhys.saturating_sub(st.ullAvailPhys);
            self.pct = if st.ullTotalPhys > 0 { self.used as f32 / st.ullTotalPhys as f32 } else { 0.0 };
            self.commit_total = st.ullTotalPageFile;
            self.commit_used = st.ullTotalPageFile.saturating_sub(st.ullAvailPageFile);
        }
    }
}
