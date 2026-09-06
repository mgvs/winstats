use windows::Win32::System::Power::{CallNtPowerInformation, GetSystemPowerStatus, SYSTEM_BATTERY_STATE, SYSTEM_POWER_STATUS, SystemBatteryState};

#[derive(Clone, Copy, Debug)]
pub struct Battery {
    /// 0..1
    pub level: f32,
    pub charging: bool,
    pub on_ac: bool,
    /// seconds of battery life left, if Windows knows
    pub remaining_secs: Option<u32>,
}

/// None when the machine has no battery.
pub fn read() -> Option<Battery> {
    let mut st = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut st).ok()? };
    // BatteryFlag 128 = no system battery, 255 = unknown
    if st.BatteryFlag == 128 || st.BatteryFlag == 255 || st.BatteryLifePercent == 255 {
        return None;
    }
    Some(Battery {
        level: st.BatteryLifePercent as f32 / 100.0,
        charging: st.BatteryFlag & 8 != 0,
        on_ac: st.ACLineStatus == 1,
        remaining_secs: if st.BatteryLifeTime == u32::MAX { None } else { Some(st.BatteryLifeTime) },
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BatteryDetail {
    /// mW; negative while discharging
    pub rate_mw: i32,
    pub remaining_mwh: u32,
    pub max_mwh: u32,
}

pub fn detail() -> Option<BatteryDetail> {
    let mut s = SYSTEM_BATTERY_STATE::default();
    let status = unsafe {
        CallNtPowerInformation(SystemBatteryState, None, 0, Some(&mut s as *mut _ as *mut _), std::mem::size_of::<SYSTEM_BATTERY_STATE>() as u32)
    };
    if status.is_err() || !s.BatteryPresent.as_bool() {
        return None;
    }
    Some(BatteryDetail { rate_mw: s.Rate as i32, remaining_mwh: s.RemainingCapacity, max_mwh: s.MaxCapacity })
}
