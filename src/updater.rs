//! Checks GitHub Releases for a newer version of `taskbar-audio.exe` using WinHTTP (zero external HTTP crates).
//! Security hardening:
//! - Enforces TLS 1.2/1.3 and explicit network timeouts.
//! - Restricts downloads and browser URLs strictly to GitHub domains (`github.com`, `objects.githubusercontent.com`).
//! - Verifies SHA-256 via Windows CNG (`bcrypt.dll`) whenever `SHA256SUMS.txt` is published with the release.
//! - Rejects truncated downloads exceeding the size cap and uses RAII handles.

#[cfg(feature = "self-update")]
use crate::i18n::t;
#[cfg(feature = "self-update")]
use crate::win32_raii::ComGuard;
use crate::win32_raii::OwnedWinHttp;
#[cfg(feature = "self-update")]
use crate::{diag, notify, settings};
use crate::wide;
use serde::Deserialize;
use std::ffi::c_void;
use std::fs;
use std::ptr::{null, null_mut};
#[cfg(feature = "self-update")]
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Networking::WinHttp::*;
#[cfg(any(feature = "self-update", test))]
use windows_sys::Win32::Security::Cryptography::{
    BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
    BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE,
};
#[cfg(feature = "self-update")]
use windows_sys::Win32::UI::Shell::ShellExecuteW;
#[cfg(feature = "self-update")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDNO, IDYES, MB_ICONINFORMATION, MB_SETFOREGROUND, MB_YESNOCANCEL, SW_SHOWNORMAL,
};

#[cfg(feature = "self-update")]
static CHECKING: AtomicBool = AtomicBool::new(false);
const MAX_DOWNLOAD_BYTES: usize = 25 * 1024 * 1024;

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
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub checksums_url: Option<String>,
}

/// Checks if a newer release is available on GitHub.
/// Disabled automatically when compiled with `--features store` (MSIX Store policy 10.2.5).
pub fn check_in_background(interactive: bool, on_exit_for_restart: fn()) {
    #[cfg(not(feature = "self-update"))]
    {
        let _ = (interactive, on_exit_for_restart);
    }

    #[cfg(feature = "self-update")]
    {
        if CHECKING.swap(true, Ordering::Relaxed) {
            return;
        }
        std::thread::spawn(move || {
            let _com = ComGuard::init_mta();
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
                        format!("up to date (current=v{})", env!("CARGO_PKG_VERSION")),
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
}

/// Removes any `taskbar-audio.old.exe` left behind by a previous self-update.
pub fn clean_old_binary() {
    if cfg!(feature = "store") {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("old.exe");
        if old.exists() {
            let _ = fs::remove_file(old);
        }
    }
}

#[cfg(feature = "self-update")]
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
            match download_and_replace(
                asset_url,
                info.asset_name.as_deref(),
                info.checksums_url.as_deref(),
            ) {
                Ok(()) => {
                    let _ = notify::show(tr.update_installed_title, tr.update_installed_body);
                    // Release the single-instance mutex before spawning the new binary so it never
                    // sees ERROR_ALREADY_EXISTS while the old process finishes shutting down.
                    crate::widget::release_instance_mutex();
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe)
                            .arg("--restarting")
                            .spawn();
                        on_exit_for_restart();
                    }
                }
                Err(e) => {
                    diag::event("update", format!("self-update failed: {e}"));
                    open_github_url(&info.html_url);
                }
            }
        } else {
            open_github_url(&info.html_url);
        }
    } else if choice == IDNO {
        open_github_url(&info.html_url);
    }
}

#[cfg(feature = "self-update")]
fn download_and_replace(
    url: &str,
    asset_name: Option<&str>,
    checksums_url: Option<&str>,
) -> Result<(), String> {
    if !is_allowed_github_url(url) {
        return Err("refusing to download update from untrusted host".into());
    }
    let bytes = https_get_url(url)?;
    if bytes.len() < 4096 || &bytes[0..2] != b"MZ" {
        return Err("downloaded file is not a valid Windows executable".into());
    }

    // Verify SHA-256 against SHA256SUMS.txt when present in the GitHub Release.
    if let (Some(sums_url), Some(name)) = (checksums_url, asset_name) {
        if is_allowed_github_url(sums_url) {
            let sums_bytes = https_get_url(sums_url)?;
            let sums_text = String::from_utf8_lossy(&sums_bytes);
            if let Some(expected_hex) = find_expected_sha256(&sums_text, name) {
                let actual_hex = sha256_hex(&bytes)?;
                if !actual_hex.eq_ignore_ascii_case(&expected_hex) {
                    return Err(format!(
                        "SHA-256 mismatch (expected {expected_hex}, got {actual_hex})"
                    ));
                }
                diag::event("update", format!("SHA-256 verified: {actual_hex}"));
            }
        }
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

#[cfg(feature = "self-update")]
fn open_github_url(url: &str) {
    // Prevent arbitrary URI/command execution via ShellExecuteW
    if !url.starts_with("https://github.com/") {
        diag::event("update", "blocked non-GitHub release URL");
        return;
    }
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
    let rel: Release =
        serde_json::from_slice(&body).map_err(|e| format!("invalid GitHub response: {e}"))?;

    let current = env!("CARGO_PKG_VERSION");
    if !is_newer_version(current, &rel.tag_name) {
        return Ok(None);
    }

    let arch_hint = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let exe_asset = rel
        .assets
        .iter()
        .find(|a| {
            let n = a.name.to_ascii_lowercase();
            n.ends_with(".exe") && n.contains(arch_hint)
        })
        .or_else(|| {
            rel.assets.iter().find(|a| {
                let n = a.name.to_ascii_lowercase();
                n.ends_with(".exe") && (n.contains("taskbar-audio") || n.contains("windows"))
            })
        })
        .or_else(|| {
            rel.assets
                .iter()
                .find(|a| a.name.to_ascii_lowercase().ends_with(".exe"))
        });

    let checksums_url = rel
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("SHA256SUMS.txt"))
        .map(|a| a.browser_download_url.clone());

    Ok(Some(UpdateInfo {
        version: rel.tag_name,
        html_url: rel.html_url,
        asset_name: exe_asset.map(|a| a.name.clone()),
        asset_url: exe_asset.map(|a| a.browser_download_url.clone()),
        checksums_url,
    }))
}

#[cfg(any(feature = "self-update", test))]
fn is_allowed_github_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    matches!(
        host,
        "github.com" | "objects.githubusercontent.com" | "release-assets.githubusercontent.com"
    )
}

#[cfg(any(feature = "self-update", test))]
fn find_expected_sha256(sums_text: &str, filename: &str) -> Option<String> {
    let target = filename.to_ascii_lowercase();
    for line in sums_text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let clean_name = name.trim_start_matches('*').to_ascii_lowercase();
        if clean_name == target && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Computes SHA-256 hex digest using Windows CNG (`bcrypt.dll`) with zero extra crate dependencies.
#[cfg(any(feature = "self-update", test))]
pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    unsafe {
        let mut alg: BCRYPT_ALG_HANDLE = null_mut();
        let alg_id = wide("SHA256");
        if BCryptOpenAlgorithmProvider(&mut alg, alg_id.as_ptr(), null(), 0) != 0 {
            return Err("BCryptOpenAlgorithmProvider(SHA256) failed".into());
        }
        let mut hash: BCRYPT_HASH_HANDLE = null_mut();
        let st_create = BCryptCreateHash(alg, &mut hash, null_mut(), 0, null(), 0, 0);
        if st_create != 0 {
            BCryptCloseAlgorithmProvider(alg, 0);
            return Err("BCryptCreateHash failed".into());
        }
        let st_data = if data.is_empty() {
            0
        } else {
            BCryptHashData(hash, data.as_ptr(), data.len() as u32, 0)
        };
        let mut digest = [0u8; 32];
        let st_fin = if st_data == 0 {
            BCryptFinishHash(hash, digest.as_mut_ptr(), digest.len() as u32, 0)
        } else {
            st_data
        };
        BCryptDestroyHash(hash);
        BCryptCloseAlgorithmProvider(alg, 0);
        if st_fin != 0 {
            return Err("BCryptFinishHash failed".into());
        }
        let mut hex = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{b:02x}");
        }
        Ok(hex)
    }
}

#[cfg(feature = "self-update")]
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
        let session = OwnedWinHttp(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen failed".into());
        }

        // Enforce 10s resolve/connect and 20s send/receive timeouts so network stalls never hang
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 20_000, 20_000);

        // Enforce TLS 1.2+ only
        let tls_flags: u32 = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
        let _ = WinHttpSetOption(
            session.0,
            WINHTTP_OPTION_SECURE_PROTOCOLS,
            &tls_flags as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );

        let host_w = wide(host);
        let connect = OwnedWinHttp(WinHttpConnect(
            session.0,
            host_w.as_ptr(),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if connect.0.is_null() {
            return Err("WinHttpConnect failed".into());
        }

        let verb = wide("GET");
        let path_w = wide(path);
        let request = OwnedWinHttp(WinHttpOpenRequest(
            connect.0,
            verb.as_ptr(),
            path_w.as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE | WINHTTP_FLAG_REFRESH,
        ));
        if request.0.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }

        let headers = wide("Accept: application/vnd.github+json\r\nCache-Control: no-cache\r\n");
        let headers_len = headers.len().saturating_sub(1) as u32;
        let sent = WinHttpSendRequest(
            request.0,
            headers.as_ptr(),
            headers_len,
            null(),
            0,
            0,
            0,
        );
        if sent == 0 || WinHttpReceiveResponse(request.0, null_mut()) == 0 {
            return Err("network request failed".into());
        }

        let mut status: u32 = 0;
        let mut status_len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut status_len,
            null_mut(),
        );

        let mut out = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(request.0, &mut avail) == 0 || avail == 0 {
                break;
            }
            if out.len().saturating_add(avail as usize) > MAX_DOWNLOAD_BYTES {
                return Err("download exceeded 25 MB safety limit".into());
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(
                request.0,
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
        }

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

    #[test]
    fn github_url_allowlist() {
        assert!(is_allowed_github_url(
            "https://github.com/vinny/windows-taskbar-audio/releases/download/v0.1.0/taskbar-audio.exe"
        ));
        assert!(is_allowed_github_url(
            "https://objects.githubusercontent.com/github-production-release-asset/foo"
        ));
        assert!(!is_allowed_github_url("http://github.com/evil"));
        assert!(!is_allowed_github_url("https://evil.example.com/taskbar-audio.exe"));
    }

    #[test]
    fn cng_sha256_matches_standard_vector() {
        let digest = sha256_hex(b"hello world").expect("bcrypt sha256");
        assert_eq!(
            digest,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn parses_sha256sums_file() {
        let content = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9  taskbar-audio.exe\n";
        assert_eq!(
            find_expected_sha256(content, "taskbar-audio.exe").as_deref(),
            Some("b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9")
        );
        assert_eq!(find_expected_sha256(content, "other.exe"), None);
    }
}
