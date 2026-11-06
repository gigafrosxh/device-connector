//! Welche Laufwerke dieser Rechner freigibt: alle lokalen Platten,
//! Partitionen und USB-Sticks, keine Netzlaufwerke (sonst sähe der andere
//! Rechner seine eigenen Laufwerke über Umwege wieder).

use dc_core::remotefs::Volume;

/// Namen eindeutig machen und von `/` befreien.
fn tidy(mut vols: Vec<Volume>) -> Vec<Volume> {
    let mut seen = std::collections::HashSet::new();
    for v in &mut vols {
        let base: String = v
            .name
            .chars()
            .map(|c| if matches!(c, '/' | '\\') { '_' } else { c })
            .collect();
        let base = if base.trim().is_empty() {
            "Laufwerk".to_string()
        } else {
            base.trim().to_string()
        };
        let mut name = base.clone();
        let mut n = 2;
        while !seen.insert(name.clone()) {
            name = format!("{base} {n}");
            n += 1;
        }
        v.name = name;
    }
    vols
}

#[cfg(target_os = "macos")]
pub fn list() -> Vec<Volume> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut out: Vec<Volume> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/Volumes") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(real) = std::fs::canonicalize(e.path()) else {
                continue;
            };
            let Ok(c) = std::ffi::CString::new(real.as_os_str().as_bytes()) else {
                continue;
            };
            let mut st: libc::statfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
                continue;
            }
            let on = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
            // Nur echte Einhängepunkte (leere Ordner in /Volumes überspringen).
            if std::path::Path::new(std::ffi::OsStr::from_bytes(on.to_bytes())) != real {
                continue;
            }
            let flags = st.f_flags as i64;
            if flags & libc::MNT_LOCAL as i64 == 0 || flags & libc::MNT_DONTBROWSE as i64 != 0 {
                continue;
            }
            #[allow(clippy::unnecessary_cast)]
            let bs = st.f_bsize as u64;
            #[allow(clippy::unnecessary_cast)]
            out.push(Volume {
                name,
                root: real,
                total: Some(st.f_blocks as u64 * bs),
                free: Some(st.f_bavail as u64 * bs),
            });
        }
    }
    if !out.iter().any(|v| v.root == std::path::Path::new("/")) {
        out.push(Volume {
            name: "Macintosh HD".into(),
            root: "/".into(),
            total: None,
            free: crate::platform::free_space(std::path::Path::new("/")),
        });
    }
    // Systemplatte zuerst.
    out.sort_by_key(|v| (v.root != std::path::Path::new("/"), v.name.to_lowercase()));
    tidy(out)
}

#[cfg(windows)]
pub fn list() -> Vec<Volume> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let h = HSTRING::from(root.as_str());
        let kind = unsafe { GetDriveTypeW(&h) };
        if !matches!(
            kind,
            DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_CDROM | DRIVE_RAMDISK
        ) {
            continue;
        }
        let mut label = [0u16; 261];
        // Schlägt bei leeren Kartenlesern und DVD-Laufwerken fehl: auslassen.
        if unsafe { GetVolumeInformationW(&h, Some(&mut label), None, None, None, None) }.is_err() {
            continue;
        }
        let len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
        let label = String::from_utf16_lossy(&label[..len]);
        let label = if label.trim().is_empty() {
            match kind {
                DRIVE_REMOVABLE => "USB-Laufwerk".to_string(),
                DRIVE_CDROM => "DVD-Laufwerk".to_string(),
                _ => "Lokaler Datenträger".to_string(),
            }
        } else {
            label
        };
        let (mut avail, mut total) = (0u64, 0u64);
        let sizes =
            unsafe { GetDiskFreeSpaceExW(&h, Some(&mut avail), Some(&mut total), None) }.is_ok();
        out.push(Volume {
            name: format!("{label} ({letter})"),
            root: root.into(),
            total: sizes.then_some(total),
            free: sizes.then_some(avail),
        });
    }
    tidy(out)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn list() -> Vec<Volume> {
    tidy(vec![Volume {
        name: "System".into(),
        root: "/".into(),
        total: None,
        free: crate::platform::free_space(std::path::Path::new("/")),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_without_slashes() {
        let v = |n: &str| Volume {
            name: n.into(),
            root: "/".into(),
            total: None,
            free: None,
        };
        let t = tidy(vec![v("USB"), v("USB"), v("a/b"), v("  ")]);
        let names: Vec<_> = t.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["USB", "USB 2", "a_b", "Laufwerk"]);
    }

    #[test]
    fn this_computer_has_a_volume() {
        assert!(!list().is_empty());
    }
}
