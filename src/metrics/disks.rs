//! Physical disks: model, bus and size through IOCTL_STORAGE_QUERY_PROPERTY on \\.\PhysicalDriveN.

use crate::util::{pcwstr, wide};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::*;

#[derive(Clone, Debug)]
pub struct PhysicalDisk {
    /// PhysicalDriveN index, matches the leading number of PDH "PhysicalDisk" instances ("0 C:")
    pub index: u32,
    pub model: String,
    pub bus: &'static str,
    pub size: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    /// counted in the widget and the chart (Physical disks list in the popup toggles it)
    pub enabled: bool,
}

fn bus_name(bus: u32) -> &'static str {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "IEEE 1394",
        5 => "SSA",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "FileBackedVirtual",
        16 => "Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "",
    }
}

fn cstr_at(buf: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= buf.len() {
        return String::new();
    }
    let end = buf[off..].iter().position(|b| *b == 0).map(|p| off + p).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[off..end]).trim().to_string()
}

fn query(index: u32) -> Option<PhysicalDisk> {
    let path = wide(&format!("\\\\.\\PhysicalDrive{index}"));
    let h: HANDLE = unsafe {
        CreateFileW(pcwstr(&path), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None).ok()?
    };
    let result = unsafe {
        let q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
        let mut buf = vec![0u8; 4096];
        let mut got = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut got),
            None,
        )
        .is_ok();
        if !ok || (got as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
            None
        } else {
            let d = &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR);
            let vendor = cstr_at(&buf, d.VendorIdOffset);
            let product = cstr_at(&buf, d.ProductIdOffset);
            let model = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
            // IOCTL_DISK_GET_LENGTH_INFO needs read access (admin); the geometry query does not
            let mut geo = DISK_GEOMETRY_EX::default();
            let mut got2 = 0u32;
            let size = if DeviceIoControl(
                h,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                None,
                0,
                Some(&mut geo as *mut _ as *mut _),
                std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
                Some(&mut got2),
                None,
            )
            .is_ok()
            {
                geo.DiskSize.max(0) as u64
            } else {
                0
            };
            // mounted VHD / Dev Drive images are not hardware
            let virtual_disk = matches!(d.BusType.0, 14 | 15);
            if virtual_disk {
                None
            } else {
                Some(PhysicalDisk {
                    index,
                    model: if model.is_empty() { format!("Disk {index}") } else { model },
                    bus: bus_name(d.BusType.0 as u32),
                    size,
                    read_bps: 0.0,
                    write_bps: 0.0,
                    enabled: true,
                })
            }
        }
    };
    unsafe {
        let _ = CloseHandle(h);
    }
    result
}

/// Apply the user's switched-off list; when it would leave nothing on, everything stays on.
pub fn with_disabled(mut disks: Vec<PhysicalDisk>, disabled: &[String]) -> Vec<PhysicalDisk> {
    for d in &mut disks {
        d.enabled = !disabled.contains(&d.model);
    }
    if !disks.iter().any(|d| d.enabled) {
        for d in &mut disks {
            d.enabled = true;
        }
    }
    disks
}

/// All physical drives 0..32 that answer (indices can have gaps after hot-unplugs).
pub fn enumerate() -> Vec<PhysicalDisk> {
    (0..32u32).filter_map(query).collect()
}
