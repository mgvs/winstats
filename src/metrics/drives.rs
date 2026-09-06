//! Fixed, removable and optical drives with label, file system, free and total space.

use crate::util::{pcwstr, wide};
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, QueryDosDeviceW};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_CDROM: u32 = 5;

#[derive(Clone, Debug)]
pub struct Drive {
    pub letter: char,
    pub label: String,
    /// "" for fixed disks, "USB" for removable, "DVD" for optical
    pub kind: &'static str,
    /// file system name as Windows reports it: NTFS, FAT32, exFAT, UDF, CDFS
    pub fs: String,
    /// the volume cannot be written to (optical media, write-protected cards)
    pub readonly: bool,
    pub free: u64,
    pub total: u64,
}

const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

/// The drive letter maps to `\Device\Floppy<n>` (QueryDosDevice reads the object manager only,
/// the hardware stays idle).
fn is_floppy(letter: char) -> bool {
    let name = wide(&format!("{letter}:"));
    let mut target = [0u16; 256];
    let n = unsafe { QueryDosDeviceW(pcwstr(&name), Some(&mut target)) } as usize;
    if n == 0 {
        // no answer: the classic floppy letters are still not worth a spin-up
        return letter == 'A' || letter == 'B';
    }
    String::from_utf16_lossy(&target[..n.min(target.len())]).to_ascii_lowercase().contains("\\device\\floppy")
}

pub fn list() -> Vec<Drive> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = wide(&format!("{letter}:\\"));
        unsafe {
            let kind = match GetDriveTypeW(pcwstr(&root)) {
                DRIVE_FIXED => "",
                // floppy drives are "removable" too; asking them for free space spins the motor
                // every second, so they are recognised by their device name and skipped without
                // touching them
                DRIVE_REMOVABLE if is_floppy(letter) => continue,
                DRIVE_REMOVABLE => "USB",
                DRIVE_CDROM => "DVD",
                _ => continue,
            };
            let mut free = 0u64;
            let mut total = 0u64;
            // fails for an empty card reader / optical drive: skip those
            if GetDiskFreeSpaceExW(pcwstr(&root), None, Some(&mut total), Some(&mut free)).is_err() || total == 0 {
                continue;
            }
            let mut name = [0u16; 64];
            let mut fs_name = [0u16; 32];
            let mut flags = 0u32;
            let _ = GetVolumeInformationW(pcwstr(&root), Some(&mut name), None, None, Some(&mut flags), Some(&mut fs_name));
            let label = String::from_utf16_lossy(&name).trim_end_matches('\0').to_string();
            let fs = String::from_utf16_lossy(&fs_name).trim_end_matches('\0').to_string();
            let readonly = kind == "DVD" || flags & FILE_READ_ONLY_VOLUME != 0;
            out.push(Drive { letter, label, kind, fs, readonly, free, total });
        }
    }
    out
}
