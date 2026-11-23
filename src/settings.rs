//! Registry-backed settings: start-with-Windows, language choice, taskbar device label,
//! theme detection, and update repository override.

use crate::i18n::Lang;
use crate::wide;
use std::ffi::c_void;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_KEY: &str = r"Software\taskbar-audio";
const RUN_VALUE: &str = "TaskbarAudio";
const STARTUP_APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const APPROVED_ON: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

pub const DEFAULT_GITHUB_REPO: &str = "vinny/windows-taskbar-audio";

pub fn light_theme() -> bool {
    read_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "SystemUsesLightTheme",
    ) == Some(1)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Autostart {
    On,
    NoRunEntry,
    DisabledByWindows,
}

impl std::fmt::Display for Autostart {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Autostart::On => "on",
            Autostart::NoRunEntry => "off (no Run entry)",
            Autostart::DisabledByWindows => "off (turned off in Windows' Startup apps)",
        })
    }
}

pub fn autostart() -> Autostart {
    if !run_entry_present() {
        Autostart::NoRunEntry
    } else if read_binary(STARTUP_APPROVED_KEY, RUN_VALUE).is_some_and(|b| is_disabled_flag(&b)) {
        Autostart::DisabledByWindows
    } else {
        Autostart::On
    }
}

pub fn autostart_enabled() -> bool {
    autostart() == Autostart::On
}

fn run_entry_present() -> bool {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    let mut size = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        ) == ERROR_SUCCESS
    }
}

fn is_disabled_flag(approved: &[u8]) -> bool {
    approved.first().is_some_and(|b| b & 1 == 1)
}

fn write_run_entry(on: bool) {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_SZ,
                data.as_ptr() as *const c_void,
                (data.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
}

pub fn set_autostart(on: bool) {
    write_run_entry(on);
    let (k, v) = (wide(STARTUP_APPROVED_KEY), wide(RUN_VALUE));
    unsafe {
        if on {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_BINARY,
                APPROVED_ON.as_ptr() as *const c_void,
                APPROVED_ON.len() as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
    write_dword(APP_KEY, "AutostartOptOut", u32::from(!on));
}

pub fn ensure_autostart() {
    if cfg!(debug_assertions) || read_dword(APP_KEY, "AutostartOptOut") == Some(1) {
        return;
    }
    write_run_entry(true);
}

pub fn welcomed() -> bool {
    read_dword(APP_KEY, "Welcomed") == Some(1)
}

pub fn set_welcomed() {
    write_dword(APP_KEY, "Welcomed", 1);
}

/// Whether to show a short device name next to the volume percentage on the taskbar.
/// Enabled by default.
pub fn show_device_name() -> bool {
    read_dword(APP_KEY, "ShowDeviceName") != Some(0)
}

pub fn set_show_device_name(on: bool) {
    write_dword(APP_KEY, "ShowDeviceName", u32::from(on));
}

/// Returns `Some(Lang)` if the user explicitly picked a language, or `None` for Automatic (Windows UI language).
pub fn language_override() -> Option<Lang> {
    read_string(APP_KEY, "Language").and_then(|s| Lang::from_code(&s))
}

pub fn active_lang() -> Lang {
    language_override().unwrap_or_else(Lang::system_default)
}

pub fn set_language_override(lang: Option<Lang>) {
    match lang {
        Some(l) => write_string(APP_KEY, "Language", l.code()),
        None => delete_value(APP_KEY, "Language"),
    }
}

pub fn github_repo() -> String {
    if let Ok(env_repo) = std::env::var("TASKBAR_AUDIO_REPO") {
        if !env_repo.trim().is_empty() {
            return env_repo;
        }
    }
    read_string(APP_KEY, "GitHubRepo")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string())
}

const NOTIFICATION_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon-64.png"));

pub fn register_app_id() {
    let key = format!(r"Software\Classes\AppUserModelId\{}", crate::notify::APP_ID);
    write_string(&key, "DisplayName", "Taskbar Audio");
    if let Some(dir) = std::env::var_os("LOCALAPPDATA")
        .map(|d| std::path::PathBuf::from(d).join("taskbar-audio"))
    {
        let icon = dir.join("icon.png");
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&icon, NOTIFICATION_ICON).is_ok()
        {
            write_string(&key, "IconUri", &icon.to_string_lossy());
        }
    }
}

pub fn windows_version() -> String {
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build: u32 = read_string_in(HKEY_LOCAL_MACHINE, KEY, "CurrentBuild")
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let ubr = read_dword_in(HKEY_LOCAL_MACHINE, KEY, "UBR").unwrap_or(0);
    let release = read_string_in(HKEY_LOCAL_MACHINE, KEY, "DisplayVersion").unwrap_or_default();
    let name = if build >= 22000 { "Windows 11" } else { "Windows 10" };
    format!("{name} {release} build {build}.{ubr}")
}

fn write_string(key: &str, name: &str, value: &str) {
    let (k, n, data) = (wide(key), wide(name), wide(value));
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ptr(),
            REG_SZ,
            data.as_ptr() as *const c_void,
            (data.len() * 2) as u32,
        );
    }
}

fn delete_value(key: &str, name: &str) {
    let (k, n) = (wide(key), wide(name));
    unsafe {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr());
    }
}

fn read_string(key: &str, name: &str) -> Option<String> {
    read_string_in(HKEY_CURRENT_USER, key, name)
}

fn read_string_in(root: HKEY, key: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(key), wide(name));
    let mut size = 0u32;
    unsafe {
        if RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        if RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

fn read_binary(key: &str, name: &str) -> Option<Vec<u8>> {
    let (k, n) = (wide(key), wide(name));
    let mut buf = [0u8; 64];
    let mut size = buf.len() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then(|| buf[..size as usize].to_vec())
}

fn read_dword(key: &str, name: &str) -> Option<u32> {
    read_dword_in(HKEY_CURRENT_USER, key, name)
}

fn read_dword_in(root: HKEY, key: &str, name: &str) -> Option<u32> {
    let (k, n) = (wide(key), wide(name));
    let (mut value, mut size) = (0u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut value as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then_some(value)
}

fn write_dword(key: &str, name: &str, value: u32) {
    let (k, n) = (wide(key), wide(name));
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ptr(),
            REG_DWORD,
            &value as *const u32 as *const c_void,
            4,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_approved_flag() {
        assert!(!is_disabled_flag(&APPROVED_ON));
        assert!(!is_disabled_flag(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(is_disabled_flag(&[
            0x03, 0, 0, 0, 0x8a, 0x3c, 0x1d, 0x5e, 0x2f, 0x5b, 0xdb, 0x01
        ]));
        assert!(is_disabled_flag(&[0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!is_disabled_flag(&[]));
    }
}
