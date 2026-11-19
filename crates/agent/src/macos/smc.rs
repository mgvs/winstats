//! The System Management Controller through the AppleSMC user client: fan speeds, the power the
//! whole Mac draws and, on Intel Macs, the integrated GPU's temperature. No root needed.

use super::{Io, IoObject};
use winstats_proto::FanSample;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceOpen(service: IoObject, task: u32, kind: u32, conn: *mut IoObject) -> i32;
    fn IOServiceClose(conn: IoObject) -> i32;
    fn IOConnectCallStructMethod(conn: IoObject, selector: u32, input: *const KeyData, in_size: usize, output: *mut KeyData, out_size: *mut usize) -> i32;
}

/// `SMCKeyData_t` of the AppleSMC kext, 80 bytes; the padding stands for the alignment of its
/// nested structs.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyData {
    key: u32,
    vers: [u8; 6],
    _pad0: [u8; 2],
    plimit: [u8; 16],
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
    _pad1: [u8; 3],
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

const _: () = assert!(std::mem::size_of::<KeyData>() == 80);

const KERNEL_INDEX: u32 = 2;
const READ_KEY: u8 = 5;
const GET_KEY_INFO: u8 = 9;

fn code(s: &str) -> u32 {
    s.bytes().take(4).fold(0, |a, b| (a << 8) | b as u32)
}

pub struct Smc {
    conn: IoObject,
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe { IOServiceClose(self.conn) };
    }
}

impl Smc {
    pub fn open() -> Option<Smc> {
        let svc = Io::services("AppleSMC").into_iter().next()?;
        let mut conn = 0;
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self() };
        (unsafe { IOServiceOpen(svc.0, task, 0, &mut conn) } == 0 && conn != 0).then_some(Smc { conn })
    }

    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut out = KeyData::default();
        let mut size = std::mem::size_of::<KeyData>();
        let r = unsafe { IOConnectCallStructMethod(self.conn, KERNEL_INDEX, input, size, &mut out, &mut size) };
        (r == 0 && out.result == 0).then_some(out)
    }

    /// Raw bytes and the type ("flt ", "fpe2", "sp78", "ui8 ") of a key.
    fn raw(&self, key: &str) -> Option<(Vec<u8>, u32)> {
        let info = self.call(&KeyData { key: code(key), data8: GET_KEY_INFO, ..Default::default() })?;
        let n = (info.data_size as usize).min(32);
        if n == 0 {
            return None;
        }
        let out = self.call(&KeyData { key: code(key), data_size: info.data_size, data8: READ_KEY, ..Default::default() })?;
        Some((out.bytes[..n].to_vec(), info.data_type))
    }

    /// A key as a number, whatever its encoding; None when the Mac has no such key.
    pub fn read(&self, key: &str) -> Option<f64> {
        let (b, t) = self.raw(key)?;
        let be16 = || u16::from_be_bytes([b[0], *b.get(1).unwrap_or(&0)]);
        let v = match t {
            t if t == code("flt ") && b.len() >= 4 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            t if t == code("fpe2") => be16() as f64 / 4.0,
            t if t == code("sp78") => be16() as i16 as f64 / 256.0,
            t if t == code("ui8 ") => b[0] as f64,
            t if t == code("ui16") => be16() as f64,
            t if t == code("ui32") && b.len() >= 4 => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            _ => return None,
        };
        v.is_finite().then_some(v)
    }

    pub fn fans(&self) -> Vec<FanSample> {
        let n = self.read("FNum").unwrap_or(0.0) as usize;
        (0..n.min(8))
            .filter_map(|i| {
                let rpm = self.read(&format!("F{i}Ac"))?;
                Some(FanSample { id: format!("Fan {}", i + 1), rpm: Some(rpm.max(0.0) as u32), duty: None })
            })
            .collect()
    }

    /// What the whole Mac draws, W ("PSTR"); 0 is not a reading.
    pub fn system_power(&self) -> Option<f32> {
        self.read("PSTR").filter(|w| *w > 0.0 && *w < 2000.0).map(|w| w as f32)
    }

    /// The battery pack's temperature ("TB0T".."TB2T"): Apple silicon keeps it out of the
    /// battery's own properties.
    pub fn battery_temp(&self) -> Option<f32> {
        ["TB0T", "TB1T", "TB2T"].iter().find_map(|k| self.read(k)).filter(|c| *c > -20.0 && *c < 90.0).map(|c| c as f32)
    }

    /// Intel graphics through PECI ("TCGC") or the GPU diode ("TG0D", "TG0P").
    pub fn gpu_temp(&self) -> Option<f32> {
        ["TCGC", "TG0D", "TG0P"].iter().find_map(|k| self.read(k)).filter(|c| *c > 0.0 && *c < 150.0).map(|c| c as f32)
    }
}
