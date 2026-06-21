//! Retrieve system info

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

fn reg_sz(subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    unsafe {
        let mut buf = [0u16; 512];
        let mut cb = (buf.len() * 2) as u32;
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut cb),
        );
        if rc.0 != 0 {
            return None;
        }
        let len = (cb as usize / 2).saturating_sub(1); // drop trailing NUL
        Some(String::from_utf16_lossy(&buf[..len]).trim().to_string())
    }
}

fn reg_dword(subkey: PCWSTR, value: PCWSTR) -> Option<u32> {
    unsafe {
        let mut data = 0u32;
        let mut cb = 4u32;
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut cb),
        );
        (rc.0 == 0).then_some(data)
    }
}

fn cpu_name() -> Option<String> {
    reg_sz(
        w!("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"),
        w!("ProcessorNameString"),
    )
}

fn ram_gb() -> Option<u64> {
    unsafe {
        let mut m = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut m).ok()?;
        Some((m.ullTotalPhys as f64 / (1024.0 * 1024.0 * 1024.0)).round() as u64)
    }
}

fn os_version() -> Option<String> {
    let cv = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let build: u32 = reg_sz(cv, w!("CurrentBuildNumber"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let release = if build >= 22000 { "11" } else { "10" };
    let display = reg_sz(cv, w!("DisplayVersion")).unwrap_or_default();
    let ubr = reg_dword(cv, w!("UBR")).unwrap_or(0);
    Some(format!("Windows {release} {display} | Build: {build}.{ubr}"))
}

pub fn tested_on(device: &str) -> Vec<String> {
    let unknown = || "unknown".to_string();
    vec![
        "Tested on".to_string(),
        format!("CPU: {}", cpu_name().unwrap_or_else(unknown)),
        format!("RAM: {} GB", ram_gb().map(|g| g.to_string()).unwrap_or_else(unknown)),
        format!("OS: {}", os_version().unwrap_or_else(unknown)),
        format!("Audio Device: {device}"),
        "Test Method: Loopback".to_string(),
    ]
}
