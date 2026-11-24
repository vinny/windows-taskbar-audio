//! Window plumbing for the taskbar widget:
//! - Hidden top-level `TaskbarAudioController` receives system broadcasts (`TaskbarCreated`,
//!   `WM_SETTINGCHANGE`, `WM_DEVICECHANGE`, `WM_POWERBROADCAST`), owns the placement timer and context menu.
//! - Visible `TaskbarAudioWidget` is a layered child window of `Shell_TrayWnd`, placed just left
//!   of `TrayNotifyWnd` (or left of an existing taskbar widget if another one is running).
//! - Worker thread polls Core Audio state and wakes immediately on scroll, click, or device changes.

use crate::audio::{self, AudioSnapshot};
use crate::i18n::{t, Lang};
use crate::{diag, init_com, mixer, notify, render, settings, updater, wide};
use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize,
};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_MENU: u32 = WM_APP + 2;
const WM_APP_MIXER: u32 = WM_APP + 3;
const WM_APP_EXIT: u32 = WM_APP + 4;
const TIMER_PLACE: usize = 1;

const CMD_TOGGLE_MUTE: usize = 1;
const CMD_OPEN_MIXER: usize = 2;
const CMD_SHOW_DEVICE_NAME: usize = 3;
const CMD_AUTOSTART: usize = 4;
const CMD_CHECK_UPDATES: usize = 5;
const CMD_COPY_DIAGNOSTICS: usize = 6;
const CMD_EXIT: usize = 7;
const CMD_LANG_AUTO: usize = 50;
const CMD_LANG_BASE: usize = 51;
const CMD_OUTPUT_BASE: usize = 100;
const CMD_INPUT_BASE: usize = 300;

const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const POLL_AUDIO: Duration = Duration::from_secs(2);

static SNAPSHOT: Mutex<Option<AudioSnapshot>> = Mutex::new(None);
static WAKE: OnceLock<Sender<()>> = OnceLock::new();
static INSTANCE_MUTEX: AtomicUsize = AtomicUsize::new(0);
pub static DEMO: AtomicBool = AtomicBool::new(false);

struct App {
    controller: HWND,
    widget: HWND,
    tooltip: HWND,
    taskbar_created: u32,
    light: bool,
    size: (i32, i32),
    layout: Option<render::Layout>,
}

thread_local! {
    static APP: RefCell<App> = const {
        RefCell::new(App {
            controller: null_mut(),
            widget: null_mut(),
            tooltip: null_mut(),
            taskbar_created: 0,
            light: false,
            size: (0, 0),
            layout: None,
        })
    };
}

fn with<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

pub fn run() {
    unsafe {
        let mutex_name = wide(r"Local\taskbar-audio");
        let is_restarting = std::env::args().any(|a| a == "--restarting");
        let mut acquired = false;
        let attempts = if is_restarting { 40 } else { 1 };
        for _ in 0..attempts {
            let h = CreateMutexW(null(), 0, mutex_name.as_ptr());
            if GetLastError() != ERROR_ALREADY_EXISTS {
                INSTANCE_MUTEX.store(h as usize, Ordering::SeqCst);
                acquired = true;
                break;
            }
            if !h.is_null() {
                CloseHandle(h);
            }
            if is_restarting {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if !acquired {
            init_com();
            let tr = t();
            let _ = notify::show(tr.already_running_title, tr.already_running_body);
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        settings::ensure_autostart();
        settings::register_app_id();
        updater::clean_old_binary();
        diag::start();
        diag::event("autostart", settings::autostart());

        if !settings::welcomed() {
            std::thread::spawn(|| {
                init_com();
                let tr = t();
                match notify::show(tr.welcome_title, tr.welcome_body) {
                    Ok(()) => settings::set_welcomed(),
                    Err(e) => diag::event("notify", format!("first-run toast failed: {e}")),
                }
            });
        }

        let hinst = GetModuleHandleW(null());
        let ctrl_class = wide("TaskbarAudioController");
        register(hinst, &ctrl_class, controller_proc);
        register(hinst, &wide("TaskbarAudioWidget"), widget_proc);
        mixer::register_class();

        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let controller = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            ctrl_class.as_ptr(),
            ctrl_class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        with(|a| {
            a.controller = controller;
            a.taskbar_created = taskbar_created;
            a.light = settings::light_theme();
        });

        let (tx, rx) = mpsc::channel();
        let _ = WAKE.set(tx);
        let controller_addr = controller as usize;
        std::thread::spawn(move || worker(controller_addr, rx));

        ensure_widget();
        SetTimer(controller, TIMER_PLACE, 1000, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn register(
    hinst: HINSTANCE,
    class: &[u16],
    proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) {
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(proc_),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);
}

fn worker(controller: usize, rx: Receiver<()>) {
    if DEMO.load(Ordering::Relaxed) {
        *SNAPSHOT.lock().unwrap() = Some(audio::demo_snapshot());
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        return;
    }
    init_com();
    let mut watcher = audio::AudioEventWatcher::new(wake);

    loop {
        if let Some(w) = watcher.as_mut() {
            w.rebind_default_endpoint();
        }
        if let Some(snap) = audio::snapshot() {
            *SNAPSHOT.lock().unwrap() = Some(snap);
            unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        }
        trim_working_set();

        match rx.recv_timeout(POLL_AUDIO) {
            Ok(()) => {
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn trim_working_set() {
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

pub fn wake() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

fn current_snapshot() -> AudioSnapshot {
    SNAPSHOT.lock().unwrap().clone().unwrap_or_default()
}

unsafe fn find_tray() -> HWND {
    FindWindowW(wide("Shell_TrayWnd").as_ptr(), null())
}

unsafe fn ensure_widget() {
    let widget = with(|a| a.widget);
    if !widget.is_null() && IsWindow(widget) != 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    let class = wide("TaskbarAudioWidget");
    let w = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        null(),
        WS_CHILD | WS_CLIPSIBLINGS,
        0,
        0,
        1,
        1,
        tray,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    let ex = GetWindowLongW(w, GWL_EXSTYLE) as u32;
    if ex & WS_EX_LAYOUTRTL != 0 {
        SetWindowLongW(w, GWL_EXSTYLE, (ex & !WS_EX_LAYOUTRTL) as i32);
    }
    let (controller, old_tip) = with(|a| (a.controller, a.tooltip));
    if !old_tip.is_null() {
        DestroyWindow(old_tip);
    }
    let tip = create_tooltip(controller, w);
    with(|a| {
        a.widget = w;
        a.tooltip = tip;
        a.size = (0, 0);
        a.layout = None;
    });
    redraw();
}

unsafe fn create_tooltip(owner: HWND, widget: HWND) -> HWND {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_WIN95_CLASSES,
    };
    InitCommonControlsEx(&icc);
    let tip = CreateWindowExW(
        WS_EX_TOPMOST,
        wide("tooltips_class32").as_ptr(),
        null(),
        WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if tip.is_null() {
        return tip;
    }
    let mut text = wide("");
    let mut ti = tool_info(widget, owner, &mut text);
    SendMessageW(tip, TTM_ADDTOOLW, 0, &mut ti as *mut _ as LPARAM);
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 600);
    tip
}

unsafe fn tool_info(widget: HWND, owner: HWND, text: &mut [u16]) -> TTTOOLINFOW {
    let mut ti: TTTOOLINFOW = zeroed();
    ti.cbSize = size_of::<TTTOOLINFOW>() as u32;
    ti.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    ti.hwnd = owner;
    ti.uId = widget as usize;
    ti.lpszText = text.as_mut_ptr();
    ti
}

unsafe fn set_tooltip_text(lines: &[String]) {
    let (tip, widget, controller) = with(|a| (a.tooltip, a.widget, a.controller));
    if tip.is_null() {
        return;
    }
    let mut text = wide(&lines.join("\r\n"));
    let ti = tool_info(widget, controller, &mut text);
    SendMessageW(tip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
}

unsafe fn redraw() {
    let (widget, light) = with(|a| (a.widget, a.light));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    let layout = taskbar_layout(tray);
    let dpi = GetDpiForWindow(tray).max(96);
    let snap = current_snapshot();
    let tr = t();

    let mut tip_lines = Vec::new();
    let vol_str = if snap.master_muted {
        format!("{}% ({})", snap.master_volume, tr.muted)
    } else {
        format!("{}%", snap.master_volume)
    };
    tip_lines.push(format!("{}: {}", snap.device_name, vol_str));
    if snap.in_communication {
        if let Some(app) = &snap.comm_app_name {
            tip_lines.push(format!("{}: {app}", tr.comm_tooltip_prefix));
        } else {
            tip_lines.push(tr.comm_tooltip_prefix.to_string());
        }
    }
    for app in snap.sessions.iter().filter(|s| !s.is_system_sounds).take(5) {
        let a_vol = if app.muted {
            format!("{}% ({})", app.volume, tr.muted)
        } else {
            format!("{}%", app.volume)
        };
        tip_lines.push(format!("• {}: {}", app.name, a_vol));
    }
    set_tooltip_text(&tip_lines);

    let show_dev = settings::show_device_name();
    let size = render::draw(widget, &snap, show_dev, dpi, layout, light);
    with(|a| {
        a.size = size;
        a.layout = Some(layout);
    });
    place();
    mixer::refresh_if_visible(snap, light);
}

unsafe fn taskbar_layout(tray: HWND) -> render::Layout {
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let (w, h) = (tr.right - tr.left, tr.bottom - tr.top);
    if h > w {
        render::Layout::Column { width: w }
    } else {
        render::Layout::Row { height: h }
    }
}

/// Positions the widget left of `TrayNotifyWnd` (or left of `TaskbarBatteryWidget` if the user runs both!).
unsafe fn place() {
    let (widget, (width, height), rendered) = with(|a| (a.widget, a.size, a.layout));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    if rendered != Some(taskbar_layout(tray)) {
        redraw();
        return;
    }
    if width == 0 {
        ShowWindow(widget, SW_HIDE);
        return;
    }

    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let mut nr: RECT = zeroed();
    let notify = FindWindowExW(tray, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
    let has_notify = !notify.is_null() && GetWindowRect(notify, &mut nr) != 0 && nr.right > nr.left;

    // Also check if TaskbarBatteryWidget is running on the same taskbar so they sit side by side without overlapping
    let battery_sibling =
        FindWindowExW(tray, null_mut(), wide("TaskbarBatteryWidget").as_ptr(), null());
    let mut br: RECT = zeroed();
    let has_battery = !battery_sibling.is_null()
        && IsWindowVisible(battery_sibling) != 0
        && GetWindowRect(battery_sibling, &mut br) != 0
        && br.right > br.left;

    let margin = 4 * GetDpiForWindow(tray).max(96) as i32 / 96;
    let rtl = GetWindowLongW(tray, GWL_EXSTYLE) as u32 & WS_EX_LAYOUTRTL != 0;
    let client_left = |r: &RECT| if rtl { tr.right - r.right } else { r.left - tr.left };

    let (x, y) = match rendered {
        Some(render::Layout::Column { .. }) => {
            let mut bottom = if has_notify { nr.top } else { tr.bottom };
            if has_battery && br.top < bottom {
                bottom = br.top;
            }
            (0, bottom - tr.top - height - margin)
        }
        _ => {
            let mut edge = if has_notify {
                client_left(&nr)
            } else {
                tr.right - tr.left
            };
            if has_battery {
                let bat_edge = client_left(&br);
                if bat_edge > 0 && bat_edge < edge {
                    edge = bat_edge;
                }
            }
            (edge - width - margin, 0)
        }
    };

    let mut cur: RECT = zeroed();
    GetWindowRect(widget, &mut cur);
    let moved = client_left(&cur) != x
        || cur.top - tr.top != y
        || cur.right - cur.left != width
        || cur.bottom - cur.top != height;
    let on_top = GetWindow(tray, GW_CHILD) == widget;
    if moved || !on_top || IsWindowVisible(widget) == 0 {
        SetWindowPos(
            widget,
            HWND_TOP,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

unsafe fn show_menu(hwnd: HWND) {
    mixer::hide();
    let snap = current_snapshot();
    let tr = t();
    let menu = CreatePopupMenu();
    let mut labels = Vec::new();
    let keep = |labels: &mut Vec<Vec<u16>>, s: &str| -> *const u16 {
        let w = wide(s);
        let ptr = w.as_ptr();
        labels.push(w);
        ptr
    };

    // 1. Quick Mute / Unmute + Open App Mixer
    let mute_label = if snap.master_muted {
        format!("{} ({}%)", tr.unmute_now, snap.master_volume)
    } else {
        format!("{} ({}%)", tr.mute_now, snap.master_volume)
    };
    AppendMenuW(
        menu,
        MF_STRING | if snap.master_muted { MF_CHECKED } else { MF_UNCHECKED },
        CMD_TOGGLE_MUTE,
        keep(&mut labels, &mute_label),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        CMD_OPEN_MIXER,
        keep(&mut labels, tr.open_mixer),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    // 2. Output devices list
    AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED,
        0,
        keep(&mut labels, tr.output_devices_header),
    );
    if snap.outputs.is_empty() {
        AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED,
            0,
            keep(&mut labels, tr.no_output_devices),
        );
    } else {
        for (i, dev) in snap.outputs.iter().enumerate() {
            let check = if dev.is_default {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
            AppendMenuW(
                menu,
                MF_STRING | check,
                CMD_OUTPUT_BASE + i,
                keep(&mut labels, &dev.name),
            );
        }
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    // 2b. Input devices (Microphone) submenu
    let input_menu = CreatePopupMenu();
    if snap.inputs.is_empty() {
        AppendMenuW(
            input_menu,
            MF_STRING | MF_GRAYED,
            0,
            keep(&mut labels, tr.no_input_devices),
        );
    } else {
        for (i, mic) in snap.inputs.iter().enumerate() {
            let check = if mic.is_default {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
            AppendMenuW(
                input_menu,
                MF_STRING | check,
                CMD_INPUT_BASE + i,
                keep(&mut labels, &mic.name),
            );
        }
    }
    AppendMenuW(
        menu,
        MF_POPUP,
        input_menu as usize,
        keep(&mut labels, tr.input_devices_menu),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    // 3. Language submenu
    let lang_menu = CreatePopupMenu();
    let current_override = settings::language_override();
    AppendMenuW(
        lang_menu,
        MF_STRING
            | if current_override.is_none() {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            },
        CMD_LANG_AUTO,
        keep(&mut labels, tr.language_auto),
    );
    AppendMenuW(lang_menu, MF_SEPARATOR, 0, null());
    for (idx, lang) in Lang::ALL.iter().enumerate() {
        let checked = if current_override == Some(*lang) {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        AppendMenuW(
            lang_menu,
            MF_STRING | checked,
            CMD_LANG_BASE + idx,
            keep(&mut labels, lang.label()),
        );
    }
    AppendMenuW(
        menu,
        MF_POPUP,
        lang_menu as usize,
        keep(&mut labels, tr.language_menu),
    );

    // 4. Settings & actions
    let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    AppendMenuW(
        menu,
        MF_STRING | checked(settings::show_device_name()),
        CMD_SHOW_DEVICE_NAME,
        keep(&mut labels, tr.show_device_name_on_bar),
    );
    AppendMenuW(
        menu,
        MF_STRING | checked(settings::autostart_enabled()),
        CMD_AUTOSTART,
        keep(&mut labels, tr.start_with_windows),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let check_updates_label = format!("{} (v{})", tr.check_updates, env!("CARGO_PKG_VERSION"));
    AppendMenuW(
        menu,
        MF_STRING,
        CMD_CHECK_UPDATES,
        keep(&mut labels, &check_updates_label),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        CMD_COPY_DIAGNOSTICS,
        keep(&mut labels, tr.copy_diagnostics),
    );
    AppendMenuW(menu, MF_STRING, CMD_EXIT, keep(&mut labels, tr.exit));

    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        null(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);

    match cmd as usize {
        0 => {}
        CMD_TOGGLE_MUTE => {
            audio::toggle_master_mute();
            if let Some(s) = SNAPSHOT.lock().unwrap().as_mut() {
                s.master_muted = !s.master_muted;
            }
            redraw();
            wake();
        }
        CMD_OPEN_MIXER => toggle_mixer_flyout(),
        CMD_SHOW_DEVICE_NAME => {
            settings::set_show_device_name(!settings::show_device_name());
            redraw();
        }
        CMD_AUTOSTART => {
            settings::set_autostart(!settings::autostart_enabled());
        }
        CMD_CHECK_UPDATES => {
            updater::check_in_background(true, request_exit);
        }
        CMD_COPY_DIAGNOSTICS => copy_diagnostics(hwnd),
        CMD_EXIT => request_exit(),
        CMD_LANG_AUTO => {
            settings::set_language_override(None);
            redraw();
        }
        c if (CMD_LANG_BASE..CMD_LANG_BASE + Lang::ALL.len()).contains(&c) => {
            let lang = Lang::ALL[c - CMD_LANG_BASE];
            settings::set_language_override(Some(lang));
            redraw();
        }
        c if (CMD_OUTPUT_BASE..CMD_INPUT_BASE).contains(&c) => {
            if let Some(dev) = snap.outputs.get(c - CMD_OUTPUT_BASE) {
                if audio::set_default_output_device(&dev.id) {
                    diag::event("switch_output", format!("switched to {}", dev.short_name));
                }
                wake();
            }
        }
        c if c >= CMD_INPUT_BASE => {
            if let Some(mic) = snap.inputs.get(c - CMD_INPUT_BASE) {
                if audio::set_default_output_device(&mic.id) {
                    diag::event("switch_input", format!("switched mic to {}", mic.name));
                }
                wake();
            }
        }
        _ => {}
    }
}

pub fn release_instance_mutex() {
    let h = INSTANCE_MUTEX.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            CloseHandle(h as HANDLE);
        }
    }
}

fn request_exit() {
    release_instance_mutex();
    let controller = with(|a| a.controller);
    if !controller.is_null() {
        unsafe { PostMessageW(controller, WM_APP_EXIT, 0, 0) };
    }
}

unsafe fn toggle_mixer_flyout() {
    let (widget, light) = with(|a| (a.widget, a.light));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    let mut rc: RECT = zeroed();
    GetWindowRect(widget, &mut rc);
    let snap = if DEMO.load(Ordering::Relaxed) {
        current_snapshot()
    } else {
        audio::snapshot().unwrap_or_else(current_snapshot)
    };
    *SNAPSHOT.lock().unwrap() = Some(snap.clone());
    mixer::toggle(rc, tray, snap, light, || {
        wake();
    });
}

unsafe fn copy_diagnostics(hwnd: HWND) {
    let tr = t();
    let (title, body) = match diag::report() {
        Some(text) if set_clipboard(hwnd, &text) => (tr.diag_copied_title, tr.diag_copied_body),
        Some(_) => (tr.diag_copy_failed_title, tr.diag_copy_failed_body),
        None => (tr.diag_empty_title, tr.diag_empty_body),
    };
    std::thread::spawn(move || {
        init_com();
        let _ = notify::show(title, body);
    });
}

unsafe fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let units: Vec<u16> = text
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    if OpenClipboard(hwnd) == 0 {
        return false;
    }
    let mut ok = false;
    let mem = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
    if !mem.is_null() {
        let dst = GlobalLock(mem) as *mut u16;
        if !dst.is_null() {
            std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
            GlobalUnlock(mem);
            EmptyClipboard();
            ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
        }
        if !ok {
            GlobalFree(mem);
        }
    }
    CloseClipboard();
    ok
}

unsafe extern "system" fn controller_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let taskbar_created = with(|a| a.taskbar_created);
    match msg {
        WM_APP_UPDATE => redraw(),
        WM_TIMER => {
            ensure_widget();
            place();
        }
        WM_SETTINGCHANGE => {
            with(|a| a.light = settings::light_theme());
            redraw();
        }
        WM_DEVICECHANGE => {
            wake();
            return 1;
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC {
                wake();
            }
            return 1;
        }
        WM_APP_MENU => show_menu(hwnd),
        WM_APP_MIXER => toggle_mixer_flyout(),
        WM_APP_EXIT => {
            mixer::hide();
            let widget = with(|a| a.widget);
            if !widget.is_null() {
                DestroyWindow(widget);
            }
            DestroyWindow(hwnd);
        }
        WM_DESTROY => PostQuitMessage(0),
        m if m == taskbar_created && m != 0 => {
            with(|a| a.widget = null_mut());
            ensure_widget();
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

unsafe extern "system" fn widget_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONUP => {
            let controller = with(|a| a.controller);
            PostMessageW(controller, WM_APP_MIXER, 0, 0);
            0
        }
        WM_MBUTTONUP => {
            audio::toggle_master_mute();
            if let Some(s) = SNAPSHOT.lock().unwrap().as_mut() {
                s.master_muted = !s.master_muted;
            }
            let controller = with(|a| a.controller);
            PostMessageW(controller, WM_APP_UPDATE, 0, 0);
            wake();
            0
        }
        WM_RBUTTONUP => {
            let controller = with(|a| a.controller);
            PostMessageW(controller, WM_APP_MENU, 0, 0);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) as i16) as i32;
            let step = if delta > 0 { 2 } else { -2 };
            audio::step_master_volume(step);
            if let Some(s) = SNAPSHOT.lock().unwrap().as_mut() {
                s.master_volume = (s.master_volume as i32 + step).clamp(0, 100) as u8;
                if step > 0 && s.master_volume > 0 {
                    s.master_muted = false;
                }
            }
            let controller = with(|a| a.controller);
            PostMessageW(controller, WM_APP_UPDATE, 0, 0);
            wake();
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
