//! Quick Audio Mixer flyout window opened on left-click.
//!
//! Features:
//! - Multi-monitor aware positioning (`MonitorFromWindow` + `GetMonitorInfoW` `rcWork`).
//! - Dynamic resizing with max height cap so dozens of open apps never overflow off-screen.
//! - Full keyboard accessibility: `Tab` / `Up` / `Down` to select rows, `Left` / `Right` to adjust volume,
//!   `M` / `Space` to mute, `Esc` to close, plus Windows High Contrast mode support (`SPI_GETHIGHCONTRAST`).
//! - Zero GDI leaks via RAII wrappers (`OwnedDc`, `OwnedGdi`, `SelectedObj`) and clean `DestroyIcon` on shutdown.

use crate::audio::{self, AudioSnapshot, DeviceKind};
use crate::i18n::t;
use crate::render::device_icon;
use crate::wide;
use crate::win32_raii::{OwnedDc, OwnedGdi, SelectedObj};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const FLYOUT_CLASS: &str = "TaskbarAudioMixerFlyout";
const TIMER_LIVE_METER: usize = 77;
const MAX_VISIBLE_APPS: usize = 10;
const MAX_ICON_CACHE: usize = 64;

#[derive(Clone, PartialEq, Eq, Debug)]
enum Target {
    Master,
    Session(String),
}

#[derive(Clone)]
struct HitRow {
    target: Target,
    mute_rect: RECT,
    slider_rect: RECT,
    row_rect: RECT,
}

#[derive(Clone, Copy)]
struct ThemePalette {
    bg: u32,
    border: u32,
    divider: u32,
    fg: u32,
    sub: u32,
    track_bg: u32,
    accent: u32,
    peak_green: u32,
    muted: u32,
    comm_badge_bg: u32,
    comm_badge_fg: u32,
    focus_ring: u32,
}

impl ThemePalette {
    unsafe fn detect(light: bool) -> Self {
        if is_high_contrast() {
            return Self {
                bg: GetSysColor(COLOR_WINDOW),
                border: GetSysColor(COLOR_WINDOWTEXT),
                divider: GetSysColor(COLOR_GRAYTEXT),
                fg: GetSysColor(COLOR_WINDOWTEXT),
                sub: GetSysColor(COLOR_WINDOWTEXT),
                track_bg: GetSysColor(COLOR_BTNSHADOW),
                accent: GetSysColor(COLOR_HIGHLIGHT),
                peak_green: GetSysColor(COLOR_HIGHLIGHT),
                muted: GetSysColor(COLOR_GRAYTEXT),
                comm_badge_bg: GetSysColor(COLOR_HIGHLIGHT),
                comm_badge_fg: GetSysColor(COLOR_HIGHLIGHTTEXT),
                focus_ring: GetSysColor(COLOR_HIGHLIGHT),
            };
        }
        Self {
            bg: rgb(if light { 0xF7F8FA } else { 0x1F2024 }),
            border: rgb(if light { 0xD8DCE3 } else { 0x34363D }),
            divider: rgb(if light { 0xE3E6EC } else { 0x2D2F36 }),
            fg: rgb(if light { 0x18191C } else { 0xF3F4F6 }),
            sub: rgb(if light { 0x5E636E } else { 0x9CA3AF }),
            track_bg: rgb(if light { 0xD5D9E0 } else { 0x3A3D45 }),
            accent: rgb(if light { 0x0067C0 } else { 0x4CC2FF }),
            peak_green: rgb(if light { 0x107C41 } else { 0x34D399 }),
            muted: rgb(if light { 0xC42B1C } else { 0xFF6B6B }),
            comm_badge_bg: rgb(if light { 0xDFF6DD } else { 0x133829 }),
            comm_badge_fg: rgb(if light { 0x0F7B0F } else { 0x50E3C2 }),
            focus_ring: rgb(if light { 0x005A9E } else { 0x60CDFF }),
        }
    }
}

struct SliderItem<'a> {
    title: &'a str,
    app_icon: HICON,
    volume: u8,
    muted: bool,
    peak: f32,
    is_comm: bool,
    focused: bool,
    fill_col: u32,
    row: &'a HitRow,
}

struct FlyoutState {
    hwnd: HWND,
    anchor_rect: RECT,
    tray_hwnd: HWND,
    dpi: u32,
    light: bool,
    snap: AudioSnapshot,
    rows: Vec<HitRow>,
    dragging: Option<Target>,
    focused_row: Option<usize>,
    on_change: Option<fn()>,
    icon_cache: HashMap<String, HICON>,
}

thread_local! {
    static STATE: RefCell<FlyoutState> = RefCell::new(FlyoutState {
        hwnd: null_mut(),
        anchor_rect: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        tray_hwnd: null_mut(),
        dpi: 96,
        light: false,
        snap: AudioSnapshot::default(),
        rows: Vec::new(),
        dragging: None,
        focused_row: None,
        on_change: None,
        icon_cache: HashMap::new(),
    });
}

fn with_state<R>(f: impl FnOnce(&mut FlyoutState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

pub unsafe fn register_class() {
    let hinst = GetModuleHandleW(null());
    let class = wide(FLYOUT_CLASS);
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(flyout_proc),
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

pub unsafe fn is_visible() -> bool {
    let hwnd = with_state(|s| s.hwnd);
    !hwnd.is_null() && IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) != 0
}

pub unsafe fn hide() {
    let hwnd = with_state(|s| {
        s.dragging = None;
        s.focused_row = None;
        s.hwnd
    });
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        KillTimer(hwnd, TIMER_LIVE_METER);
        ShowWindow(hwnd, SW_HIDE);
    }
}

/// Frees all cached `HICON` handles and destroys the flyout window on app exit.
pub unsafe fn cleanup() {
    hide();
    let (hwnd, icons) = with_state(|st| {
        let icons: Vec<HICON> = st.icon_cache.drain().map(|(_, h)| h).collect();
        let hwnd = st.hwnd;
        st.hwnd = null_mut();
        (hwnd, icons)
    });
    for h in icons {
        if !h.is_null() {
            DestroyIcon(h);
        }
    }
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        DestroyWindow(hwnd);
    }
}

/// Toggles the mixer flyout anchored near `anchor_rect` (the widget's screen rectangle).
pub unsafe fn toggle(
    anchor_rect: RECT,
    tray_hwnd: HWND,
    snap: AudioSnapshot,
    light: bool,
    on_change: fn(),
) {
    if is_visible() {
        hide();
        return;
    }

    let hwnd = ensure_hwnd();
    if hwnd.is_null() {
        return;
    }

    let dpi = GetDpiForWindow(tray_hwnd).max(96);
    with_state(|st| {
        st.anchor_rect = anchor_rect;
        st.tray_hwnd = tray_hwnd;
        st.dpi = dpi;
        st.light = light;
        st.snap = snap;
        st.dragging = None;
        st.focused_row = None;
        st.on_change = Some(on_change);
    });

    position_and_resize(hwnd, true);
    InvalidateRect(hwnd, null(), 1);
    SetForegroundWindow(hwnd);
    SetTimer(hwnd, TIMER_LIVE_METER, 120, None);
}

/// Updates the open flyout with fresh audio data while the user is not dragging a slider.
pub unsafe fn refresh_if_visible(snap: AudioSnapshot, light: bool) {
    if !is_visible() {
        return;
    }
    let (hwnd, count_changed) = with_state(|st| {
        if st.dragging.is_some() {
            return (null_mut(), false);
        }
        let prev_len = st.snap.sessions.len().min(MAX_VISIBLE_APPS);
        let next_len = snap.sessions.len().min(MAX_VISIBLE_APPS);
        st.snap = snap;
        st.light = light;
        let s = |v: i32| v * st.dpi as i32 / 96;
        rebuild_hit_rows(st, s(340));
        (st.hwnd, prev_len != next_len)
    });
    if !hwnd.is_null() {
        if count_changed {
            position_and_resize(hwnd, false);
        }
        InvalidateRect(hwnd, null(), 0);
    }
}

unsafe fn position_and_resize(hwnd: HWND, show: bool) {
    let (anchor_rect, tray_hwnd, dpi, app_count) = with_state(|st| {
        (
            st.anchor_rect,
            st.tray_hwnd,
            st.dpi,
            st.snap.sessions.len().clamp(1, MAX_VISIBLE_APPS),
        )
    });
    let s = |v: i32| v * dpi as i32 / 96;
    let width = s(340);
    let height = s(134) + (app_count as i32 * s(54)) + s(16);

    with_state(|st| rebuild_hit_rows(st, width));

    // Multi-monitor aware work area via MonitorFromWindow + GetMonitorInfoW
    let monitor = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    let work = if GetMonitorInfoW(monitor, &mut mi) != 0 {
        mi.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    };

    let mut tray_rc: RECT = zeroed();
    if !tray_hwnd.is_null() && IsWindow(tray_hwnd) != 0 {
        GetWindowRect(tray_hwnd, &mut tray_rc);
    } else {
        tray_rc = RECT {
            left: work.left,
            top: work.bottom,
            right: work.right,
            bottom: work.bottom,
        };
    }

    let margin = s(10);
    let max_x = (work.right - width - margin).max(work.left + margin);
    let max_y = (work.bottom - height - margin).max(work.top + margin);

    let center_x = (anchor_rect.left + anchor_rect.right) / 2;
    let mut x = (center_x - width / 2).clamp(work.left + margin, max_x);
    let mut y = tray_rc.top - height - margin;
    if y < work.top + margin {
        y = (tray_rc.bottom + margin).clamp(work.top + margin, max_y);
    }
    if tray_rc.bottom - tray_rc.top > tray_rc.right - tray_rc.left {
        let mid = (work.left + work.right) / 2;
        x = if tray_rc.left < mid {
            (tray_rc.right + margin).clamp(work.left + margin, max_x)
        } else {
            (tray_rc.left - width - margin).clamp(work.left + margin, max_x)
        };
        y = (anchor_rect.bottom - height).clamp(work.top + margin, max_y);
    }

    let flags = if show { SWP_SHOWWINDOW } else { SWP_NOACTIVATE };
    SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, flags);
}

unsafe fn ensure_hwnd() -> HWND {
    let existing = with_state(|s| s.hwnd);
    if !existing.is_null() && IsWindow(existing) != 0 {
        return existing;
    }
    let class = wide(FLYOUT_CLASS);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class.as_ptr(),
        wide("Taskbar Audio Mixer").as_ptr(),
        WS_POPUP,
        0,
        0,
        340,
        240,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if !hwnd.is_null() {
        let pref: u32 = DWMWCP_ROUND as u32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &pref as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
        with_state(|s| s.hwnd = hwnd);
    }
    hwnd
}

fn rebuild_hit_rows(st: &mut FlyoutState, width: i32) {
    let s = |v: i32| v * st.dpi as i32 / 96;
    st.rows.clear();

    let pad_x = s(16);
    let mute_w = s(28);
    let slider_left = pad_x + mute_w + s(8);
    let slider_right = width - pad_x - s(42);

    let master_y = s(44);
    st.rows.push(HitRow {
        target: Target::Master,
        mute_rect: RECT {
            left: pad_x,
            top: master_y + s(18),
            right: pad_x + mute_w,
            bottom: master_y + s(46),
        },
        slider_rect: RECT {
            left: slider_left,
            top: master_y + s(18),
            right: slider_right,
            bottom: master_y + s(46),
        },
        row_rect: RECT {
            left: 0,
            top: master_y,
            right: width,
            bottom: master_y + s(52),
        },
    });

    let mut y = s(134);
    let row_h = s(54);
    for app in st.snap.sessions.iter().take(MAX_VISIBLE_APPS) {
        st.rows.push(HitRow {
            target: Target::Session(app.key.clone()),
            mute_rect: RECT {
                left: pad_x,
                top: y + s(20),
                right: pad_x + mute_w,
                bottom: y + s(48),
            },
            slider_rect: RECT {
                left: slider_left,
                top: y + s(20),
                right: slider_right,
                bottom: y + s(48),
            },
            row_rect: RECT {
                left: 0,
                top: y,
                right: width,
                bottom: y + row_h,
            },
        });
        y += row_h;
    }

    if let Some(idx) = st.focused_row {
        if idx >= st.rows.len() {
            st.focused_row = st.rows.len().checked_sub(1);
        }
    }
}

unsafe extern "system" fn flyout_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            if (wp & 0xFFFF) == WA_INACTIVE as usize {
                hide();
            }
            0
        }
        WM_KEYDOWN => {
            handle_keydown(hwnd, wp);
            0
        }
        WM_TIMER => {
            if wp == TIMER_LIVE_METER && is_visible() {
                let dragging = with_state(|s| s.dragging.is_some());
                if !dragging && !crate::widget::DEMO.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut snap_copy = with_state(|s| s.snap.clone());
                    let prev_len = snap_copy.sessions.len().min(MAX_VISIBLE_APPS);
                    if audio::refresh_sessions_and_peaks(&mut snap_copy) {
                        let next_len = snap_copy.sessions.len().min(MAX_VISIBLE_APPS);
                        with_state(|st| {
                            st.snap = snap_copy;
                            let s = |v: i32| v * st.dpi as i32 / 96;
                            rebuild_hit_rows(st, s(340));
                        });
                        if prev_len != next_len {
                            position_and_resize(hwnd, false);
                        }
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            }
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = (loword_i16(lp) as i32, hiword_i16(lp) as i32);
            handle_click(hwnd, x, y);
            0
        }
        WM_MOUSEMOVE => {
            let (x, _y) = (loword_i16(lp) as i32, hiword_i16(lp) as i32);
            handle_drag(hwnd, x);
            0
        }
        WM_LBUTTONUP => {
            let was_dragging = with_state(|s| s.dragging.take().is_some());
            if was_dragging {
                ReleaseCapture();
                notify_host_changed();
            }
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) as i16) as i32;
            let step = if delta > 0 { 2 } else { -2 };
            let mut pt = POINT {
                x: loword_i16(lp) as i32,
                y: hiword_i16(lp) as i32,
            };
            ScreenToClient(hwnd, &mut pt);
            handle_wheel(hwnd, pt.x, pt.y, step);
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_flyout(hwnd, hdc);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn handle_keydown(hwnd: HWND, vk: WPARAM) {
    const VK_TAB: usize = 0x09;
    const VK_ESCAPE: usize = 0x1B;
    const VK_SPACE: usize = 0x20;
    const VK_LEFT: usize = 0x25;
    const VK_UP: usize = 0x26;
    const VK_RIGHT: usize = 0x27;
    const VK_DOWN: usize = 0x28;
    const VK_M: usize = 0x4D;

    match vk {
        VK_ESCAPE => hide(),
        VK_TAB | VK_DOWN => {
            with_state(|st| {
                if !st.rows.is_empty() {
                    st.focused_row = Some(match st.focused_row {
                        Some(i) => (i + 1) % st.rows.len(),
                        None => 0,
                    });
                }
            });
            InvalidateRect(hwnd, null(), 0);
        }
        VK_UP => {
            with_state(|st| {
                if !st.rows.is_empty() {
                    st.focused_row = Some(match st.focused_row {
                        Some(0) | None => st.rows.len() - 1,
                        Some(i) => i - 1,
                    });
                }
            });
            InvalidateRect(hwnd, null(), 0);
        }
        VK_LEFT | VK_RIGHT => {
            let step = if vk == VK_RIGHT { 2 } else { -2 };
            let target = with_state(|st| {
                let idx = st.focused_row.unwrap_or(0);
                st.rows.get(idx).map(|r| r.target.clone())
            });
            if let Some(target) = target {
                step_target(hwnd, &target, step);
            }
        }
        VK_SPACE | VK_M => {
            let target = with_state(|st| {
                let idx = st.focused_row.unwrap_or(0);
                st.rows.get(idx).map(|r| r.target.clone())
            });
            if let Some(target) = target {
                toggle_target_mute(hwnd, &target);
            }
        }
        _ => {}
    }
}

unsafe fn handle_click(hwnd: HWND, x: i32, y: i32) {
    let hit = with_state(|st| {
        for (idx, r) in st.rows.iter().enumerate() {
            if pt_in_rect(&r.mute_rect, x, y) {
                st.focused_row = Some(idx);
                return Some((r.target.clone(), None));
            }
            if pt_in_rect(&r.slider_rect, x, y) {
                st.focused_row = Some(idx);
                let pct = slider_percent(&r.slider_rect, x);
                return Some((r.target.clone(), Some(pct)));
            }
        }
        None
    });

    let Some((target, slider_pct)) = hit else {
        return;
    };

    match slider_pct {
        None => toggle_target_mute(hwnd, &target),
        Some(pct) => {
            SetCapture(hwnd);
            with_state(|st| st.dragging = Some(target.clone()));
            apply_slider_target(&target, pct);
            InvalidateRect(hwnd, null(), 0);
            notify_host_changed();
        }
    }
}

unsafe fn toggle_target_mute(hwnd: HWND, target: &Target) {
    match target {
        Target::Master => {
            audio::toggle_master_mute();
            with_state(|st| st.snap.master_muted = !st.snap.master_muted);
        }
        Target::Session(key) => {
            audio::toggle_session_mute(key);
            with_state(|st| {
                if let Some(app) = st.snap.sessions.iter_mut().find(|a| &a.key == key) {
                    app.muted = !app.muted;
                }
            });
        }
    }
    InvalidateRect(hwnd, null(), 0);
    notify_host_changed();
}

unsafe fn step_target(hwnd: HWND, target: &Target, step: i32) {
    let new_pct = with_state(|st| match target {
        Target::Master => (st.snap.master_volume as i32 + step).clamp(0, 100) as u8,
        Target::Session(key) => st
            .snap
            .sessions
            .iter()
            .find(|a| &a.key == key)
            .map(|a| (a.volume as i32 + step).clamp(0, 100) as u8)
            .unwrap_or(100),
    });
    apply_slider_target(target, new_pct);
    InvalidateRect(hwnd, null(), 0);
    notify_host_changed();
}

unsafe fn handle_drag(hwnd: HWND, x: i32) {
    let update = with_state(|st| {
        let target = st.dragging.clone()?;
        let row = st.rows.iter().find(|r| r.target == target)?;
        Some((target, slider_percent(&row.slider_rect, x)))
    });
    if let Some((target, pct)) = update {
        apply_slider_target(&target, pct);
        InvalidateRect(hwnd, null(), 0);
        notify_host_changed();
    }
}

unsafe fn handle_wheel(hwnd: HWND, x: i32, y: i32, step: i32) {
    let target = with_state(|st| {
        st.rows
            .iter()
            .find(|r| pt_in_rect(&r.row_rect, x, y))
            .map(|r| r.target.clone())
            .unwrap_or(Target::Master)
    });
    step_target(hwnd, &target, step);
}

fn apply_slider_target(target: &Target, pct: u8) {
    match target {
        Target::Master => {
            audio::set_master_volume(pct);
            with_state(|st| {
                st.snap.master_volume = pct;
                if pct > 0 {
                    st.snap.master_muted = false;
                }
            });
        }
        Target::Session(key) => {
            audio::set_session_volume(key, pct);
            with_state(|st| {
                if let Some(app) = st.snap.sessions.iter_mut().find(|a| &a.key == key) {
                    app.volume = pct;
                    if pct > 0 {
                        app.muted = false;
                    }
                }
            });
        }
    }
}

fn notify_host_changed() {
    let cb = with_state(|s| s.on_change);
    if let Some(f) = cb {
        f();
    }
}

fn pt_in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

fn slider_percent(r: &RECT, x: i32) -> u8 {
    let span = (r.right - r.left).max(1);
    let pos = (x - r.left).clamp(0, span);
    ((pos * 100 + span / 2) / span).clamp(0, 100) as u8
}

const fn rgb(hex: u32) -> u32 {
    let r = (hex >> 16) & 0xFF;
    let g = (hex >> 8) & 0xFF;
    let b = hex & 0xFF;
    (b << 16) | (g << 8) | r
}

unsafe fn is_high_contrast() -> bool {
    #[repr(C)]
    struct HighContrastW {
        cb_size: u32,
        dw_flags: u32,
        lpsz_default_scheme: *mut u16,
    }
    const HCF_HIGHCONTRASTON: u32 = 0x0000_0001;
    let mut hc = HighContrastW {
        cb_size: size_of::<HighContrastW>() as u32,
        dw_flags: 0,
        lpsz_default_scheme: null_mut(),
    };
    SystemParametersInfoW(
        SPI_GETHIGHCONTRAST,
        hc.cb_size,
        &mut hc as *mut _ as *mut c_void,
        0,
    ) != 0
        && (hc.dw_flags & HCF_HIGHCONTRASTON) != 0
}

unsafe fn paint_flyout(hwnd: HWND, target_dc: HDC) {
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return;
    }

    let dc = OwnedDc(CreateCompatibleDC(target_dc));
    if dc.0.is_null() {
        return;
    }
    let bmp = OwnedGdi(CreateCompatibleBitmap(target_dc, w, h));
    if bmp.0.is_null() {
        return;
    }
    let _bmp_guard = SelectedObj::select(dc.0, bmp.0);

    let (dpi, light, snap, rows, focused_row) = with_state(|st| {
        (
            st.dpi,
            st.light,
            st.snap.clone(),
            st.rows.clone(),
            st.focused_row,
        )
    });
    let s = |v: i32| v * dpi as i32 / 96;
    let tr = t();
    let palette = ThemePalette::detect(light);

    let bg_brush = OwnedGdi(CreateSolidBrush(palette.bg));
    FillRect(dc.0, &rc, bg_brush.0);

    let border_brush = OwnedGdi(CreateSolidBrush(palette.border));
    FrameRect(dc.0, &rc, border_brush.0);

    let title_font = OwnedGdi(make_font("Segoe UI", -s(13), FW_SEMIBOLD as i32));
    let label_font = OwnedGdi(make_font("Segoe UI", -s(12), FW_NORMAL as i32));
    let small_font = OwnedGdi(make_font("Segoe UI", -s(11), FW_SEMIBOLD as i32));
    let icon_font = OwnedGdi(make_font("Segoe Fluent Icons", -s(16), FW_NORMAL as i32));

    // Guard restores the DC's original HFONT before any OwnedGdi(HFONT) is dropped!
    let _font_guard = SelectedObj::select(dc.0, label_font.0);
    SetBkMode(dc.0, TRANSPARENT as _);

    let pad_x = s(16);

    // Header: active device icon + full device name + optional "In call" badge
    SelectObject(dc.0, icon_font.0);
    SetTextColor(
        dc.0,
        if snap.master_muted {
            palette.muted
        } else {
            palette.fg
        },
    );
    let dev_glyph = wide(device_icon(
        snap.device_kind,
        snap.master_volume,
        snap.master_muted,
    ));
    TextOutW(
        dc.0,
        pad_x,
        s(14),
        dev_glyph.as_ptr(),
        dev_glyph.len() as i32 - 1,
    );

    let badge_reserve = if snap.in_communication { s(92) } else { 0 };
    SelectObject(dc.0, title_font.0);
    SetTextColor(dc.0, palette.fg);
    let mut header_rc = RECT {
        left: pad_x + s(24),
        top: s(13),
        right: w - pad_x - badge_reserve,
        bottom: s(35),
    };
    let mut dev_title = wide(&snap.device_name);
    DrawTextW(
        dc.0,
        dev_title.as_mut_ptr(),
        -1,
        &mut header_rc,
        DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );

    if snap.in_communication {
        let badge_text = snap
            .comm_app_name
            .as_deref()
            .unwrap_or(tr.comm_active_badge);
        let badge_rc = RECT {
            left: w - pad_x - s(86),
            top: s(12),
            right: w - pad_x,
            bottom: s(32),
        };
        fill_round_rect(dc.0, &badge_rc, s(6), palette.comm_badge_bg);
        SelectObject(dc.0, small_font.0);
        SetTextColor(dc.0, palette.comm_badge_fg);
        let mut bw = wide(badge_text);
        let mut trc = badge_rc;
        DrawTextW(
            dc.0,
            bw.as_mut_ptr(),
            -1,
            &mut trc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }

    // Master Volume Row
    if let Some(master_row) = rows.first() {
        let item = SliderItem {
            title: tr.mixer_master_label,
            app_icon: null_mut(),
            volume: snap.master_volume,
            muted: snap.master_muted,
            peak: 0.0,
            is_comm: false,
            focused: focused_row == Some(0),
            fill_col: palette.accent,
            row: master_row,
        };
        draw_slider_row(dc.0, &s, w, &item, label_font.0, icon_font.0, &palette);
    }

    // Divider + "Active apps" section title
    let div_rc = RECT {
        left: pad_x,
        top: s(102),
        right: w - pad_x,
        bottom: s(103),
    };
    let div_brush = OwnedGdi(CreateSolidBrush(palette.divider));
    FillRect(dc.0, &div_rc, div_brush.0);

    SelectObject(dc.0, small_font.0);
    SetTextColor(dc.0, palette.sub);
    let apps_hdr = wide(tr.mixer_apps_header);
    TextOutW(
        dc.0,
        pad_x,
        s(110),
        apps_hdr.as_ptr(),
        apps_hdr.len() as i32 - 1,
    );

    if snap.sessions.is_empty() {
        SelectObject(dc.0, label_font.0);
        SetTextColor(dc.0, palette.sub);
        let mut empty_rc = RECT {
            left: pad_x,
            top: s(144),
            right: w - pad_x,
            bottom: s(180),
        };
        let mut empty_w = wide(tr.mixer_no_apps);
        DrawTextW(
            dc.0,
            empty_w.as_mut_ptr(),
            -1,
            &mut empty_rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    } else {
        for (idx, app) in snap.sessions.iter().take(MAX_VISIBLE_APPS).enumerate() {
            if let Some(hit_row) = rows.get(idx + 1) {
                let app_icon = app
                    .exe_path
                    .as_deref()
                    .map(|p| get_cached_app_icon(p))
                    .unwrap_or(null_mut());
                let item = SliderItem {
                    title: &app.name,
                    app_icon,
                    volume: app.volume,
                    muted: app.muted,
                    peak: app.peak,
                    is_comm: app.is_comm_app && app.is_active,
                    focused: focused_row == Some(idx + 1),
                    fill_col: if app.is_comm_app && app.is_active {
                        palette.comm_badge_fg
                    } else {
                        palette.accent
                    },
                    row: hit_row,
                };
                draw_slider_row(dc.0, &s, w, &item, label_font.0, icon_font.0, &palette);
            }
        }
    }

    BitBlt(target_dc, 0, 0, w, h, dc.0, 0, 0, SRCCOPY);
}

unsafe fn get_cached_app_icon(exe_path: &str) -> HICON {
    if let Some(cached) = with_state(|st| st.icon_cache.get(exe_path).copied()) {
        return cached;
    }
    let path_w = wide(exe_path);
    let mut small_icon: HICON = null_mut();
    let count = ExtractIconExW(path_w.as_ptr(), 0, null_mut(), &mut small_icon, 1);
    let hicon = if count > 0 { small_icon } else { null_mut() };
    let evicted = with_state(|st| {
        let mut evicted = Vec::new();
        if st.icon_cache.len() >= MAX_ICON_CACHE {
            evicted.extend(st.icon_cache.drain().map(|(_, h)| h));
        }
        st.icon_cache.insert(exe_path.to_string(), hicon);
        evicted
    });
    for old in evicted {
        if !old.is_null() {
            DestroyIcon(old);
        }
    }
    hicon
}

unsafe fn draw_slider_row(
    dc: HDC,
    s: &dyn Fn(i32) -> i32,
    width: i32,
    item: &SliderItem<'_>,
    label_font: HFONT,
    icon_font: HFONT,
    palette: &ThemePalette,
) {
    let pad_x = s(16);
    let row = item.row;

    if item.focused {
        let focus_rc = RECT {
            left: s(6),
            top: row.row_rect.top,
            right: width - s(6),
            bottom: row.row_rect.bottom - s(2),
        };
        let focus_brush = OwnedGdi(CreateSolidBrush(palette.focus_ring));
        FrameRect(dc, &focus_rc, focus_brush.0);
    }

    let mut title_left = row.slider_rect.left;
    if !item.app_icon.is_null() {
        let icon_sz = s(16);
        DrawIconEx(
            dc,
            title_left,
            row.row_rect.top + s(3),
            item.app_icon,
            icon_sz,
            icon_sz,
            0,
            null_mut(),
            DI_NORMAL,
        );
        title_left += icon_sz + s(6);
    }

    // Title above the slider
    SelectObject(dc, label_font);
    SetTextColor(dc, if item.muted { palette.sub } else { palette.fg });
    let label_str = if item.is_comm {
        format!("● {}", item.title)
    } else {
        item.title.to_string()
    };
    let mut title_rc = RECT {
        left: title_left,
        top: row.row_rect.top + s(2),
        right: width - pad_x - s(44),
        bottom: row.row_rect.top + s(22),
    };
    let mut title_w = wide(&label_str);
    DrawTextW(
        dc,
        title_w.as_mut_ptr(),
        -1,
        &mut title_rc,
        DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );

    // Mute button icon on the left
    SelectObject(dc, icon_font);
    SetTextColor(dc, if item.muted { palette.muted } else { palette.fg });
    let glyph = wide(device_icon(DeviceKind::Speakers, item.volume, item.muted));
    TextOutW(
        dc,
        row.mute_rect.left + s(4),
        row.mute_rect.top + s(4),
        glyph.as_ptr(),
        glyph.len() as i32 - 1,
    );

    // Slider track
    let track_h = s(6);
    let cy = (row.slider_rect.top + row.slider_rect.bottom) / 2;
    let track_rc = RECT {
        left: row.slider_rect.left,
        top: cy - track_h / 2,
        right: row.slider_rect.right,
        bottom: cy + track_h / 2,
    };
    fill_round_rect(dc, &track_rc, s(3), palette.track_bg);

    let span = (track_rc.right - track_rc.left).max(1);
    let fill_w = (span * item.volume.min(100) as i32) / 100;
    let active_color = if item.muted {
        palette.muted
    } else {
        item.fill_col
    };
    if fill_w > 0 {
        let fill_rc = RECT {
            left: track_rc.left,
            top: track_rc.top,
            right: track_rc.left + fill_w,
            bottom: track_rc.bottom,
        };
        fill_round_rect(dc, &fill_rc, s(3), active_color);
    }

    // Live audio peak bar right below the slider track when sound is actively playing
    if item.peak > 0.01 && !item.muted {
        let peak_w =
            ((span as f32) * item.peak.min(1.0) * (item.volume as f32 / 100.0)).round() as i32;
        if peak_w > 2 {
            let peak_rc = RECT {
                left: track_rc.left,
                top: track_rc.bottom + s(3),
                right: (track_rc.left + peak_w).min(track_rc.right),
                bottom: track_rc.bottom + s(5),
            };
            let br = OwnedGdi(CreateSolidBrush(palette.peak_green));
            FillRect(dc, &peak_rc, br.0);
        }
    }

    // Slider thumb circle
    let thumb_x = track_rc.left + fill_w;
    let r = s(7);
    let brush = OwnedGdi(CreateSolidBrush(active_color));
    let pen = OwnedGdi(CreatePen(PS_SOLID, 1, active_color));
    let _b_guard = SelectedObj::select(dc, brush.0);
    let _p_guard = SelectedObj::select(dc, pen.0);
    Ellipse(dc, thumb_x - r, cy - r, thumb_x + r, cy + r);

    // Percentage readout on the right
    SelectObject(dc, label_font);
    SetTextColor(dc, if item.muted { palette.muted } else { palette.sub });
    let pct_str = if item.muted {
        String::from("0%")
    } else {
        format!("{}%", item.volume)
    };
    let mut pct_rc = RECT {
        left: row.slider_rect.right + s(6),
        top: row.slider_rect.top,
        right: width - pad_x,
        bottom: row.slider_rect.bottom,
    };
    let mut pct_w = wide(&pct_str);
    DrawTextW(
        dc,
        pct_w.as_mut_ptr(),
        -1,
        &mut pct_rc,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}

unsafe fn fill_round_rect(dc: HDC, rc: &RECT, radius: i32, color: u32) {
    let brush = OwnedGdi(CreateSolidBrush(color));
    let pen = OwnedGdi(CreatePen(PS_SOLID, 1, color));
    let _b_guard = SelectedObj::select(dc, brush.0);
    let _p_guard = SelectedObj::select(dc, pen.0);
    RoundRect(dc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
}

unsafe fn make_font(face: &str, height: i32, weight: i32) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = height;
    lf.lfWeight = weight;
    lf.lfQuality = CLEARTYPE_QUALITY as _;
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    CreateFontIndirectW(&lf)
}

const fn loword_i16(lp: LPARAM) -> i16 {
    (lp & 0xFFFF) as i16
}

const fn hiword_i16(lp: LPARAM) -> i16 {
    ((lp >> 16) & 0xFFFF) as i16
}
