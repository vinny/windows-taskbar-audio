//! Support diagnostics log written to `%LOCALAPPDATA%\taskbar-audio\diagnostics.log`.
//! Records audio endpoint transitions, session state summaries, and update events without personal paths.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const MAX_LEN: u64 = 256 * 1024;
const COPY_LEN: usize = 32 * 1024;

static LAST: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
static FILE_IO_LOCK: Mutex<()> = Mutex::new(());

pub fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("taskbar-audio").join("diagnostics.log"))
}

pub fn header() -> String {
    format!(
        "taskbar-audio {} | {} | {}",
        env!("CARGO_PKG_VERSION"),
        crate::settings::windows_version(),
        std::env::consts::ARCH
    )
}

pub fn start() {
    write(&format!("--- {}", header()));
}

pub fn event(key: &str, msg: impl Display) {
    let msg = msg.to_string();
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let last = last.get_or_insert_with(HashMap::new);
        if last.get(key) == Some(&msg) {
            return;
        }
        last.insert(key.to_string(), msg.clone());
    }
    write(&format!("[{key}] {msg}"));
}

pub fn crash(msg: impl Display) {
    write(&format!("[crash] {msg}"));
}

fn write(line: &str) {
    if cfg!(debug_assertions) {
        eprintln!("{line}");
    }
    let Some(path) = path() else { return };
    let _guard = FILE_IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LEN) {
        let _ = fs::rename(&path, path.with_extension("old.log"));
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", timestamp());
    }
}

fn timestamp() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn report() -> Option<String> {
    let text = fs::read_to_string(path()?).ok()?;
    Some(format!("{}\n\n{}", header(), tail(&text, COPY_LEN)))
}

fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    match text[start..].find('\n') {
        Some(i) => &text[start + i + 1..],
        None => &text[start..],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_starts_at_a_line() {
        assert_eq!(tail("aaa\nbbb\nccc\n", 100), "aaa\nbbb\nccc\n");
        assert_eq!(tail("aaa\nbbb\nccc\n", 6), "ccc\n");
        assert_eq!(tail("ééé\nccc\n", 7), "ccc\n");
    }
}
