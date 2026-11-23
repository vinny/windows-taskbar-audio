//! Quick Audio Mixer flyout window opened on left-click.
//!
//! Renders a lightweight, native-feeling Windows 11 flyout above the taskbar widget:
//! - Active output device header + communication badge when a call is active
//! - Master volume slider + mute toggle
//! - Per-application volume sliders + mute toggles + live audio peak bars for open apps
//! - Supports dragging sliders, clicking mute buttons, and scrolling the mouse wheel over any row.

use crate::audio::{self, AudioSnapshot, DeviceKind};
use crate::i18n::t;
use crate::render::device_icon;
use crate::wide;
use std::cell::RefCell;
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
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const FLYOUT_CLASS: &str = "TaskbarAudioMixerFlyout";
const TIMER_LIVE_METER: usize = 77;

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

struct FlyoutState {
    hwnd: HWND,
    dpi: u32,
    light: bool,
    snap: AudioSnapshot,
    rows: Vec<HitRow>,
    dragging: Option<Target>,
    on_change: Option<fn()>,
}

thread_local! {
    static STATE: RefCell<FlyoutState> = RefCell::new(FlyoutState {
        hwnd: null_mut(),
        dpi: 96,
        light: false,
        snap: AudioSnapshot::default(),
        rows: Vec::new(),
        dragging: None,
        on_change: None,
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
        s.hwnd
    });
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        KillTimer(hwnd, TIMER_LIVE_METER);
        ShowWindow(hwnd, SW_HIDE);
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
    let s = |v: i32| v * dpi as i32 / 96;
    let width = s(340);
    let app_rows = snap.sessions.len().max(1);
    let height = s(134) + (app_rows as i32 * s(54)) + s(16);

    // Position the flyout centered above the widget (or clamped to the work area edges)
    let mut tray_rc: RECT = zeroed();
    GetWindowRect(tray_hwnd, &mut tray_rc);
    let margin = s(10);

    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);

    let center_x = (anchor_rect.left + anchor_rect.right) / 2;
    let mut x = (center_x - width / 2).clamp(margin, (screen_w - width - margin).max(margin));
    let mut y = tray_rc.top - height - margin;
    if y < margin {
        // Taskbar at the top of the screen
        y = tray_rc.bottom + margin;
    }
    if tray_rc.bottom - tray_rc.top > tray_rc.right - tray_rc.left {
        // Vertical taskbar docked left or right
        x = if tray_rc.left < screen_w / 2 {
            tray_rc.right + margin
        } else {
            tray_rc.left - width - margin
        };
        y = (anchor_rect.bottom - height).clamp(margin, (screen_h - height - margin).max(margin));
    }

    with_state(|st| {
        st.dpi = dpi;
        st.light = light;
        st.snap = snap;
        st.dragging = None;
        st.on_change = Some(on_change);
        rebuild_hit_rows(st, width);
    });

    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x,
        y,
        width,
        height,
        SWP_SHOWWINDOW,
    );
    InvalidateRect(hwnd, null(), 1);
    SetForegroundWindow(hwnd);
    SetTimer(hwnd, TIMER_LIVE_METER, 120, None);
}

/// Updates the open flyout with fresh audio data while the user is not dragging a slider.
pub unsafe fn refresh_if_visible(snap: AudioSnapshot, light: bool) {
    if !is_visible() {
        return;
    }
    let hwnd = with_state(|st| {
        if st.dragging.is_some() {
            return null_mut();
        }
        st.snap = snap;
        st.light = light;
        let s = |v: i32| v * st.dpi as i32 / 96;
        let width = s(340);
        rebuild_hit_rows(st, width);
        st.hwnd
    });
    if !hwnd.is_null() {
        InvalidateRect(hwnd, null(), 0);
    }
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
        // Ask Windows 11 DWM for rounded corners + drop shadow
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

    // Master row at y = s(44)..s(94)
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

    // Apps start at y = s(134)
    let mut y = s(134);
    let row_h = s(54);
    for app in &st.snap.sessions {
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
            if wp == 0x1B {
                // VK_ESCAPE
                hide();
            }
            0
        }
        WM_TIMER => {
            if wp == TIMER_LIVE_METER && is_visible() {
                let dragging = with_state(|s| s.dragging.is_some());
                if !dragging && !crate::widget::DEMO.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(fresh) = audio::snapshot() {
                        with_state(|s| s.snap = fresh);
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

unsafe fn handle_click(hwnd: HWND, x: i32, y: i32) {
    let hit = with_state(|st| {
        for r in &st.rows {
            if pt_in_rect(&r.mute_rect, x, y) {
                return Some((r.target.clone(), None));
            }
            if pt_in_rect(&r.slider_rect, x, y) {
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
        None => {
            // Clicked the mute icon
            match &target {
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
        Some(pct) => {
            SetCapture(hwnd);
            with_state(|st| st.dragging = Some(target.clone()));
            apply_slider_target(&target, pct);
            InvalidateRect(hwnd, null(), 0);
            notify_host_changed();
        }
    }
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

    let new_pct = with_state(|st| match &target {
        Target::Master => (st.snap.master_volume as i32 + step).clamp(0, 100) as u8,
        Target::Session(key) => st
            .snap
            .sessions
            .iter()
            .find(|a| &a.key == key)
            .map(|a| (a.volume as i32 + step).clamp(0, 100) as u8)
            .unwrap_or(100),
    });

    apply_slider_target(&target, new_pct);
    InvalidateRect(hwnd, null(), 0);
    notify_host_changed();
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

/// Converts 0xRRGGBB into a Win32 COLORREF (0x00BBGGRR).
const fn rgb(hex: u32) -> u32 {
    let r = (hex >> 16) & 0xFF;
    let g = (hex >> 8) & 0xFF;
    let b = hex & 0xFF;
    (b << 16) | (g << 8) | r
}

unsafe fn paint_flyout(hwnd: HWND, target_dc: HDC) {
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return;
    }

    let dc = CreateCompatibleDC(target_dc);
    let bmp = CreateCompatibleBitmap(target_dc, w, h);
    let old_bmp = SelectObject(dc, bmp);

    let (dpi, light, snap, rows) = with_state(|st| (st.dpi, st.light, st.snap.clone(), st.rows.clone()));
    let s = |v: i32| v * dpi as i32 / 96;
    let tr = t();

    // Theme palette (Windows 11 dark / light flyout style)
    let bg_col = rgb(if light { 0xF7F8FA } else { 0x1F2024 });
    let border_col = rgb(if light { 0xD8DCE3 } else { 0x34363D });
    let divider_col = rgb(if light { 0xE3E6EC } else { 0x2D2F36 });
    let fg_col = rgb(if light { 0x18191C } else { 0xF3F4F6 });
    let sub_col = rgb(if light { 0x5E636E } else { 0x9CA3AF });
    let track_bg = rgb(if light { 0xD5D9E0 } else { 0x3A3D45 });
    let accent = rgb(if light { 0x0067C0 } else { 0x4CC2FF });
    let peak_green = rgb(if light { 0x107C41 } else { 0x34D399 });
    let muted_col = rgb(if light { 0xC42B1C } else { 0xFF6B6B });
    let comm_badge_bg = rgb(if light { 0xDFF6DD } else { 0x133829 });
    let comm_badge_fg = rgb(if light { 0x0F7B0F } else { 0x50E3C2 });

    // Background + outer 1px border
    let bg_brush = CreateSolidBrush(bg_col);
    FillRect(dc, &rc, bg_brush);
    DeleteObject(bg_brush);

    let border_brush = CreateSolidBrush(border_col);
    FrameRect(dc, &rc, border_brush);
    DeleteObject(border_brush);

    let title_font = make_font("Segoe UI", -s(13), FW_SEMIBOLD as i32);
    let label_font = make_font("Segoe UI", -s(12), FW_NORMAL as i32);
    let small_font = make_font("Segoe UI", -s(11), FW_SEMIBOLD as i32);
    let icon_font = make_font("Segoe Fluent Icons", -s(16), FW_NORMAL as i32);

    SetBkMode(dc, TRANSPARENT as _);

    let pad_x = s(16);

    // Header: active device icon + full device name + optional "In call" badge
    SelectObject(dc, icon_font);
    SetTextColor(dc, if snap.master_muted { muted_col } else { fg_col });
    let dev_glyph = wide(device_icon(
        snap.device_kind,
        snap.master_volume,
        snap.master_muted,
    ));
    TextOutW(
        dc,
        pad_x,
        s(14),
        dev_glyph.as_ptr(),
        dev_glyph.len() as i32 - 1,
    );

    let badge_reserve = if snap.in_communication { s(92) } else { 0 };
    SelectObject(dc, title_font);
    SetTextColor(dc, fg_col);
    let mut header_rc = RECT {
        left: pad_x + s(24),
        top: s(13),
        right: w - pad_x - badge_reserve,
        bottom: s(35),
    };
    let mut dev_title = wide(&snap.device_name);
    DrawTextW(
        dc,
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
        fill_round_rect(dc, &badge_rc, s(6), comm_badge_bg);
        SelectObject(dc, small_font);
        SetTextColor(dc, comm_badge_fg);
        let mut bw = wide(badge_text);
        let mut trc = badge_rc;
        DrawTextW(
            dc,
            bw.as_mut_ptr(),
            -1,
            &mut trc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }

    // Master Volume Row
    if let Some(master_row) = rows.first() {
        draw_slider_row(
            dc,
            &s,
            w,
            tr.mixer_master_label,
            snap.master_volume,
            snap.master_muted,
            0.0,
            false,
            master_row,
            label_font,
            icon_font,
            fg_col,
            sub_col,
            track_bg,
            accent,
            peak_green,
            muted_col,
        );
    }

    // Divider + "Active apps" section title
    let div_rc = RECT {
        left: pad_x,
        top: s(102),
        right: w - pad_x,
        bottom: s(103),
    };
    let div_brush = CreateSolidBrush(divider_col);
    FillRect(dc, &div_rc, div_brush);
    DeleteObject(div_brush);

    SelectObject(dc, small_font);
    SetTextColor(dc, sub_col);
    let apps_hdr = wide(tr.mixer_apps_header);
    TextOutW(
        dc,
        pad_x,
        s(110),
        apps_hdr.as_ptr(),
        apps_hdr.len() as i32 - 1,
    );

    if snap.sessions.is_empty() {
        SelectObject(dc, label_font);
        SetTextColor(dc, sub_col);
        let mut empty_rc = RECT {
            left: pad_x,
            top: s(144),
            right: w - pad_x,
            bottom: s(180),
        };
        let mut empty_w = wide(tr.mixer_no_apps);
        DrawTextW(
            dc,
            empty_w.as_mut_ptr(),
            -1,
            &mut empty_rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    } else {
        for (idx, app) in snap.sessions.iter().enumerate() {
            if let Some(hit_row) = rows.get(idx + 1) {
                draw_slider_row(
                    dc,
                    &s,
                    w,
                    &app.name,
                    app.volume,
                    app.muted,
                    app.peak,
                    app.is_comm_app && app.is_active,
                    hit_row,
                    label_font,
                    icon_font,
                    fg_col,
                    sub_col,
                    track_bg,
                    if app.is_comm_app && app.is_active {
                        comm_badge_fg
                    } else {
                        accent
                    },
                    peak_green,
                    muted_col,
                );
            }
        }
    }

    BitBlt(target_dc, 0, 0, w, h, dc, 0, 0, SRCCOPY);

    DeleteObject(title_font);
    DeleteObject(label_font);
    DeleteObject(small_font);
    DeleteObject(icon_font);
    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteDC(dc);
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_slider_row(
    dc: HDC,
    s: &dyn Fn(i32) -> i32,
    width: i32,
    title: &str,
    volume: u8,
    muted: bool,
    peak: f32,
    is_comm: bool,
    row: &HitRow,
    label_font: HFONT,
    icon_font: HFONT,
    fg_col: u32,
    sub_col: u32,
    track_bg: u32,
    fill_col: u32,
    peak_col: u32,
    muted_col: u32,
) {
    let pad_x = s(16);
    // Title above the slider
    SelectObject(dc, label_font);
    SetTextColor(dc, if muted { sub_col } else { fg_col });
    let label_str = if is_comm {
        format!("● {title}")
    } else {
        title.to_string()
    };
    let mut title_rc = RECT {
        left: row.slider_rect.left,
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
    SetTextColor(dc, if muted { muted_col } else { fg_col });
    let glyph = wide(device_icon(DeviceKind::Speakers, volume, muted));
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
    fill_round_rect(dc, &track_rc, s(3), track_bg);

    let span = (track_rc.right - track_rc.left).max(1);
    let fill_w = (span * volume.min(100) as i32) / 100;
    let active_color = if muted { muted_col } else { fill_col };
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
    if peak > 0.01 && !muted {
        let peak_w = ((span as f32) * peak.min(1.0) * (volume as f32 / 100.0)).round() as i32;
        if peak_w > 2 {
            let peak_rc = RECT {
                left: track_rc.left,
                top: track_rc.bottom + s(3),
                right: (track_rc.left + peak_w).min(track_rc.right),
                bottom: track_rc.bottom + s(5),
            };
            let br = CreateSolidBrush(peak_col);
            FillRect(dc, &peak_rc, br);
            DeleteObject(br);
        }
    }

    // Slider thumb circle
    let thumb_x = track_rc.left + fill_w;
    let r = s(7);
    let brush = CreateSolidBrush(active_color);
    let pen = CreatePen(PS_SOLID, 1, active_color);
    let old_b = SelectObject(dc, brush);
    let old_p = SelectObject(dc, pen);
    Ellipse(dc, thumb_x - r, cy - r, thumb_x + r, cy + r);
    SelectObject(dc, old_b);
    SelectObject(dc, old_p);
    DeleteObject(brush);
    DeleteObject(pen);

    // Percentage readout on the right
    SelectObject(dc, label_font);
    SetTextColor(dc, if muted { muted_col } else { sub_col });
    let pct_str = if muted {
        String::from("0%")
    } else {
        format!("{volume}%")
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
    let brush = CreateSolidBrush(color);
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_b = SelectObject(dc, brush);
    let old_p = SelectObject(dc, pen);
    RoundRect(dc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
    SelectObject(dc, old_b);
    SelectObject(dc, old_p);
    DeleteObject(brush);
    DeleteObject(pen);
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
