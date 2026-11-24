#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod diag;
mod i18n;
mod mixer;
mod notify;
mod render;
mod settings;
mod updater;
mod widget;

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Initializes COM (multithreaded) on the calling thread, required for Core Audio and WinRT Toast APIs.
pub(crate) fn init_com() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
}

/// Logs panics to `%LOCALAPPDATA%\taskbar-audio\diagnostics.log` and shows a friendly crash dialog in release builds.
fn install_crash_handler() {
    static IN_HOOK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::panic::set_hook(Box::new(|info| {
        if IN_HOOK.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".into());
        let at = match info.location() {
            Some(l) if !std::path::Path::new(l.file()).is_absolute() => {
                format!(" at {}:{}", l.file(), l.line())
            }
            _ => String::new(),
        };
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let msg = format!("{what}{at} (thread {thread})");
        diag::crash(&msg);
        if cfg!(debug_assertions) {
            eprintln!("{msg}");
            return;
        }
        let log = diag::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "diagnostics.log".into());
        let text = wide(&format!(
            "Taskbar Audio stopped because of an error:\n\n{msg}\n\nPlease open an issue on GitHub and attach this file:\n{log}"
        ));
        let title = wide("Taskbar Audio");
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
            };
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
        }
    }));
}

fn main() {
    install_crash_handler();
    match std::env::args().nth(1).as_deref() {
        Some("--once") => {
            init_com();
            diag::start();
            match audio::snapshot() {
                Some(snap) => {
                    println!(
                        "Default Output : {} [{}] ({:?})",
                        snap.device_name, snap.device_short_name, snap.device_kind
                    );
                    println!(
                        "Master Volume  : {}%{}",
                        snap.master_volume,
                        if snap.master_muted { " (muted)" } else { "" }
                    );
                    println!(
                        "In Call / Comm : {}{}",
                        snap.in_communication,
                        snap.comm_app_name
                            .as_ref()
                            .map(|n| format!(" ({n})"))
                            .unwrap_or_default()
                    );
                    println!("\nAvailable Outputs ({}):", snap.outputs.len());
                    for o in &snap.outputs {
                        let mark = if o.is_default { "*" } else { " " };
                        println!("  {mark} {:<12} {}", format!("{:?}", o.kind), o.name);
                    }
                    println!("\nAvailable Microphones ({}):", snap.inputs.len());
                    for m in &snap.inputs {
                        let mark = if m.is_default { "*" } else { " " };
                        println!("  {mark} {}", m.name);
                    }
                    println!("\nActive App Sessions ({}):", snap.sessions.len());
                    for s in &snap.sessions {
                        println!(
                            "  - {:<24} {:>3}% {}{}{}",
                            s.name,
                            s.volume,
                            if s.muted { "[muted] " } else { "" },
                            if s.is_active { "[playing] " } else { "[idle] " },
                            if s.is_comm_app { "[comm]" } else { "" }
                        );
                    }
                }
                None => eprintln!("No active audio output endpoint found."),
            }
        }
        Some("--check-update") => {
            let repo = settings::github_repo();
            println!("Checking latest release on https://github.com/{repo} ...");
            match updater::fetch_latest_release(&repo) {
                Ok(Some(info)) => println!("Update available: {} ({})", info.version, info.html_url),
                Ok(None) => println!("Already up to date (v{}).", env!("CARGO_PKG_VERSION")),
                Err(e) => eprintln!("Update check failed: {e}"),
            }
        }
        Some("--demo") => {
            widget::DEMO.store(true, std::sync::atomic::Ordering::Relaxed);
            widget::run();
        }
        _ => widget::run(),
    }
}
