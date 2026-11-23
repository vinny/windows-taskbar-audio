//! Checks GitHub Releases for a newer version of `taskbar-audio.exe` using WinHTTP (zero external HTTP crates).
//! Can download the release binary, swap it in place of the running `.exe` (Windows allows renaming a running
//! executable to `.old.exe` and writing the new `.exe` in its place), and relaunch automatically.

use crate::i18n::t;
use crate::{diag, init_com, notify, settings, wide};
use serde::Deserialize;
use std::ffi::c_void;
use std::fs;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDNO, IDYES, MB_ICONINFORMATION, MB_SETFOREGROUND, MB_YESNOCANCEL, SW_SHOWNORMAL,
};

static CHECKING: AtomicBool = AtomicBool::new(false);

#[derive(Deserialize, Debug)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize, Debug)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub html_url: String,
    pub asset_url: Option<String>,
}

/// Checks if a newer release is available on GitHub.
/// When `interactive` is true (triggered from the right-click menu), shows feedback even when already up to date.
pub fn check_in_background(interactive: bool, on_exit_for_restart: fn()) {
    if CHECKING.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        init_com();
        clean_old_binary();
        if interactive {
            let tr = t();
            let _ = notify::show(tr.update_checking_title, tr.update_checking_body);
        }

        let repo = settings::github_repo();
        match fetch_latest_release(&repo) {
            Ok(Some(info)) => {
                diag::event("update", format!("new version available: {}", info.version));
                prompt_and_apply(&info, on_exit_for_restart);
            }
            Ok(None) => {
                diag::event(
                    "update",
                    format!(
                        "up to date (current=v{}, latest=v{})",
                        env!("CARGO_PKG_VERSION"),
                        env!("CARGO_PKG_VERSION")
                    ),
                );
                if interactive {
                    let tr = t();
                    let body = format!(
                        "{} (v{})",
                        tr.update_up_to_date_body,
                        env!("CARGO_PKG_VERSION")
                    );
                    let _ = notify::show(tr.update_up_to_date_title, &body);
                }
            }
            Err(e) => {
                diag::event("update", format!("check failed: {e}"));
                if interactive {
                    let tr = t();
                    let _ = notify::show(tr.update_failed_title, &e);
                }
            }
        }
        CHECKING.store(false, Ordering::Relaxed);
    });
}

/// Removes any `taskbar-audio.old.exe` left behind by a previous self-update.
pub fn clean_old_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("old.exe");
        if old.exists() {
            let _ = fs::remove_file(old);
        }
    }
}

fn prompt_and_apply(info: &UpdateInfo, on_exit_for_restart: fn()) {
    let tr = t();
    let msg = tr.update_prompt_install.replace("{ver}", &info.version);
    let text = wide(&msg);
    let title = wide(tr.update_available_title);
    let choice = unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNOCANCEL | MB_ICONINFORMATION | MB_SETFOREGROUND,
        )
    };
    if choice == IDYES {
        if let Some(asset_url) = &info.asset_url {
            match download_and_replace(asset_url) {
                Ok(()) => {
                    let _ = notify::show(tr.update_installed_title, tr.update_installed_body);
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).spawn();
                        on_exit_for_restart();
                    }
                }
                Err(e) => {
                    diag::event("update", format!("self-update failed: {e}"));
                    open_url(&info.html_url);
                }
            }
        } else {
            open_url(&info.html_url);
        }
    } else if choice == IDNO {
        open_url(&info.html_url);
    }
}

fn download_and_replace(url: &str) -> Result<(), String> {
    let bytes = https_get_url(url)?;
    if bytes.len() < 4096 || &bytes[0..2] != b"MZ" {
        return Err("downloaded file is not a valid Windows executable".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = exe.with_extension("new.exe");
    let old = exe.with_extension("old.exe");

    fs::write(&tmp, &bytes).map_err(|e| format!("failed to write update: {e}"))?;
    let _ = fs::remove_file(&old);
    fs::rename(&exe, &old).map_err(|e| format!("failed to move running exe: {e}"))?;
    if let Err(e) = fs::rename(&tmp, &exe) {
        let _ = fs::rename(&old, &exe);
        return Err(format!("failed to activate new exe: {e}"));
    }
    Ok(())
}

fn open_url(url: &str) {
    let op = wide("open");
    let u = wide(url);
    unsafe {
        ShellExecuteW(
            null_mut(),
            op.as_ptr(),
            u.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn fetch_latest_release(repo: &str) -> Result<Option<UpdateInfo>, String> {
    let path = format!("/repos/{repo}/releases/latest");
    let body = https_get("api.github.com", &path)?;
    let rel: Release = serde_json::from_slice(&body)
        .map_err(|e| format!("invalid GitHub response: {e}"))?;

    let current = env!("CARGO_PKG_VERSION");
    if !is_newer_version(current, &rel.tag_name) {
        return Ok(None);
    }

    let asset_url = rel
        .assets
        .iter()
        .find(|a| {
            let n = a.name.to_ascii_lowercase();
            n.ends_with(".exe") && (n.contains("taskbar-audio") || n.contains("windows"))
        })
        .or_else(|| rel.assets.iter().find(|a| a.name.to_ascii_lowercase().ends_with(".exe")))
        .map(|a| a.browser_download_url.clone());

    Ok(Some(UpdateInfo {
        version: rel.tag_name,
        html_url: rel.html_url,
        asset_url,
    }))
}

/// Parses a full `https://host/path` URL and performs an HTTPS GET via WinHTTP.
fn https_get_url(url: &str) -> Result<Vec<u8>, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "only https:// URLs are supported".to_string())?;
    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    https_get(host, path)
}

fn https_get(host: &str, path: &str) -> Result<Vec<u8>, String> {
    unsafe {
        let agent = wide(&format!("taskbar-audio/{}", env!("CARGO_PKG_VERSION")));
        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        );
        if session.is_null() {
            return Err("WinHttpOpen failed".into());
        }

        let host_w = wide(host);
        let connect = WinHttpConnect(session, host_w.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0);
        if connect.is_null() {
            WinHttpCloseHandle(session);
            return Err("WinHttpConnect failed".into());
        }

        let verb = wide("GET");
        let path_w = wide(path);
        let request = WinHttpOpenRequest(
            connect,
            verb.as_ptr(),
            path_w.as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE | WINHTTP_FLAG_REFRESH,
        );
        if request.is_null() {
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err("WinHttpOpenRequest failed".into());
        }

        let headers = wide("Accept: application/vnd.github+json\r\nCache-Control: no-cache\r\n");
        let headers_len = headers.len().saturating_sub(1) as u32;
        let sent = WinHttpSendRequest(
            request,
            headers.as_ptr(),
            headers_len,
            null(),
            0,
            0,
            0,
        );
        if sent == 0 || WinHttpReceiveResponse(request, null_mut()) == 0 {
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err("network request failed".into());
        }

        let mut status: u32 = 0;
        let mut status_len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut status_len,
            null_mut(),
        );

        let mut out = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(request, &mut avail) == 0 || avail == 0 {
                break;
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut c_void,
                avail,
                &mut read,
            ) == 0
                || read == 0
            {
                break;
            }
            chunk.truncate(read as usize);
            out.extend_from_slice(&chunk);
            if out.len() > 25 * 1024 * 1024 {
                break;
            }
        }

        WinHttpCloseHandle(request);
        WinHttpCloseHandle(connect);
        WinHttpCloseHandle(session);

        if status != 200 {
            return Err(format!("HTTP {status} from {host}"));
        }
        Ok(out)
    }
}

pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    parse_semver(candidate) > parse_semver(current)
}

fn parse_semver(v: &str) -> (u32, u32, u32) {
    let clean = v.trim().trim_start_matches(['v', 'V']);
    let core = clean.split(['-', '+']).next().unwrap_or(clean);
    let mut parts = core.split('.').map(|s| s.parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison() {
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.9", "v1.0.0"));
        assert!(!is_newer_version("0.2.0", "v0.2.0"));
        assert!(!is_newer_version("0.2.1", "v0.2.0"));
        assert!(is_newer_version("0.1.0", "v0.2.0-beta.1"));
    }
}
