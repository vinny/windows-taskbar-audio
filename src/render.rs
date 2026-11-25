//! Draws the taskbar widget into a per-pixel-alpha 32-bpp DIB and pushes it with `UpdateLayeredWindow`,
//! so icons and text blend cleanly over the translucent Windows 11 taskbar.
//! All GDI objects use RAII guards (`OwnedDc`, `OwnedGdi`, `SelectedObj`) so no HFONT or HBITMAP can leak.

use crate::audio::{AudioSnapshot, DeviceKind};
use crate::i18n::t;
use crate::wide;
use crate::win32_raii::{OwnedDc, OwnedGdi, SelectedObj};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

/// Segoe Fluent Icons glyphs for each audio device kind and state.
const ICON_HEADSET: &str = "\u{E7F6}";
const ICON_HEADPHONES: &str = "\u{E7F5}";
const ICON_SPEAKER_MUTE: &str = "\u{E74F}";
const ICON_SPEAKER_LOW: &str = "\u{E993}";
const ICON_SPEAKER_MID: &str = "\u{E994}";
const ICON_SPEAKER_HIGH: &str = "\u{E995}";
const ICON_DISPLAY_HDMI: &str = "\u{E7F4}";
const ICON_BLUETOOTH: &str = "\u{E702}";
/// Microphone / active communication call indicator.
const ICON_COMM_MIC: &str = "\u{E720}";

pub fn device_icon(kind: DeviceKind, volume: u8, muted: bool) -> &'static str {
    if muted || volume == 0 {
        return ICON_SPEAKER_MUTE;
    }
    match kind {
        DeviceKind::Headset => ICON_HEADSET,
        DeviceKind::Headphones => ICON_HEADPHONES,
        DeviceKind::HdmiDisplay => ICON_DISPLAY_HDMI,
        DeviceKind::Bluetooth => ICON_BLUETOOTH,
        DeviceKind::Speakers | DeviceKind::Generic => {
            if volume < 33 {
                ICON_SPEAKER_LOW
            } else if volume < 67 {
                ICON_SPEAKER_MID
            } else {
                ICON_SPEAKER_HIGH
            }
        }
    }
}

/// Captures every field that affects the taskbar bitmap so we skip redundant redraws when nothing changed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BarVisualKey {
    pub volume: u8,
    pub muted: bool,
    pub kind: DeviceKind,
    pub short_name: String,
    pub show_device_label: bool,
    pub in_communication: bool,
    pub dpi: u32,
    pub layout: Layout,
    pub light: bool,
    pub muted_label: &'static str,
}

impl BarVisualKey {
    pub fn from_state(
        snap: &AudioSnapshot,
        show_device_label: bool,
        dpi: u32,
        layout: Layout,
        light: bool,
    ) -> Self {
        Self {
            volume: snap.master_volume,
            muted: snap.master_muted,
            kind: snap.device_kind,
            short_name: if show_device_label {
                snap.device_short_name.clone()
            } else {
                String::new()
            },
            show_device_label,
            in_communication: snap.in_communication,
            dpi,
            layout,
            light,
            muted_label: t().muted,
        }
    }
}

struct Seg {
    text: Vec<u16>,
    icon: bool,
    color: u32, // 0xRRGGBB
    opacity: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap_after: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Row { height: i32 },
    Column { width: i32 },
}

pub unsafe fn draw(
    hwnd: HWND,
    snap: &AudioSnapshot,
    show_device_label: bool,
    dpi: u32,
    layout: Layout,
    light: bool,
) -> (i32, i32) {
    let s = |v: i32| v * dpi as i32 / 96;
    let fg = if light { 0x000000 } else { 0xFFFFFF };
    let muted_red = if light { 0xC42B1C } else { 0xFF6B6B };
    let comm_color = if light { 0x0F7B0F } else { 0x6CCB5F };

    let mut segs = Vec::with_capacity(4);
    let seg = |text: &str, icon: bool, color: u32, opacity: u32, gap_after: i32| Seg {
        text: wide(text),
        icon,
        color,
        opacity,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        gap_after,
    };

    let main_color = if snap.master_muted { muted_red } else { fg };
    let main_opacity = if snap.master_muted { 195 } else { 255 };

    // 1. Device / volume icon (natural taskbar theme color, matching taskbar-battery)
    let icon_glyph = device_icon(snap.device_kind, snap.master_volume, snap.master_muted);
    segs.push(seg(icon_glyph, true, main_color, main_opacity, s(5)));

    // 2. Master volume percentage (or Muted label)
    let vol_text = if snap.master_muted {
        t().muted.to_string()
    } else {
        format!("{}%", snap.master_volume)
    };
    let has_extra =
        (show_device_label && !snap.device_short_name.is_empty()) || snap.in_communication;
    segs.push(seg(
        &vol_text,
        false,
        main_color,
        main_opacity,
        if has_extra { s(6) } else { 0 },
    ));

    // 3. Optional short device name on horizontal taskbars
    if matches!(layout, Layout::Row { .. })
        && show_device_label
        && !snap.device_short_name.is_empty()
    {
        let gap = if snap.in_communication { s(6) } else { 0 };
        segs.push(seg(&snap.device_short_name, false, fg, 220, gap));
    }

    // 4. Communication mode indicator (Teams / Discord / Call active)
    if snap.in_communication {
        segs.push(seg(ICON_COMM_MIC, true, comm_color, 255, 0));
    }

    let screen = GetDC(null_mut());
    let dc = OwnedDc(CreateCompatibleDC(screen));
    ReleaseDC(null_mut(), screen);
    if dc.0.is_null() {
        return (0, 0);
    }

    let text_font = OwnedGdi(font("Segoe UI", -s(12), FW_NORMAL as i32));
    let icon_font = OwnedGdi(font("Segoe Fluent Icons", -s(15), FW_NORMAL as i32));

    // Measure segments (restore original font when scope ends so DeleteObject always succeeds)
    {
        let _font_guard = SelectedObj::select(dc.0, text_font.0);
        for item in &mut segs {
            SelectObject(dc.0, if item.icon { icon_font.0 } else { text_font.0 });
            let mut size: SIZE = zeroed();
            let char_len = item.text.len().saturating_sub(1) as i32;
            if char_len > 0 {
                GetTextExtentPoint32W(dc.0, item.text.as_ptr(), char_len, &mut size);
            }
            item.w = size.cx;
            item.h = size.cy;
        }
    }

    let pad = s(7);
    let (width, height) = match layout {
        Layout::Row { height } => {
            let mut x = pad;
            for item in &mut segs {
                item.x = x;
                item.y = (height - item.h) / 2;
                x += item.w + item.gap_after;
            }
            (x + pad, height)
        }
        Layout::Column { width } => {
            let mut y = pad;
            for item in &mut segs {
                item.x = (width - item.w) / 2;
                item.y = y;
                y += item.h + s(2);
            }
            (width, y + pad)
        }
    };

    if width <= 0 || height <= 0 {
        return (0, 0);
    }

    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB as _,
        ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = OwnedGdi(CreateDIBSection(
        dc.0,
        &bi,
        DIB_RGB_COLORS,
        &mut bits,
        null_mut(),
        0,
    ));
    if bmp.0.is_null() || bits.is_null() {
        return (width, height);
    }

    let _bmp_guard = SelectedObj::select(dc.0, bmp.0);
    {
        let _font_guard = SelectedObj::select(dc.0, text_font.0);
        SetBkMode(dc.0, TRANSPARENT as _);
        SetTextColor(dc.0, 0x00FF_FFFF);
        for item in &segs {
            let char_len = item.text.len().saturating_sub(1) as i32;
            if char_len > 0 {
                SelectObject(dc.0, if item.icon { icon_font.0 } else { text_font.0 });
                TextOutW(dc.0, item.x, item.y, item.text.as_ptr(), char_len);
            }
        }
    }
    GdiFlush();

    // Convert white-on-black grayscale coverage into premultiplied BGRA directly in place
    // without allocating a temporary `width * height` color table.
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
    // First pass: mark every pixel outside segments as clickable transparent (alpha = 1).
    for item in &segs {
        let (x0, x1) = (item.x.max(0), (item.x + item.w).min(width));
        let (y0, y1) = (item.y.max(0), (item.y + item.h).min(height));
        for y in y0..y1 {
            let row = (y * width) as usize;
            for x in x0..x1 {
                let p = &mut px[row + x as usize];
                let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
                let a = cov * item.opacity / 255;
                *p = if a == 0 {
                    0x0100_0000
                } else {
                    let ch = |shift: u32| ((item.color >> shift) & 0xFF) * a / 255;
                    (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
                };
            }
        }
    }
    // Any remaining untouched pixel (zero alpha) gets alpha = 1 so the entire bounding box is interactive.
    for p in px.iter_mut() {
        if (*p >> 24) == 0 {
            *p = 0x0100_0000;
        }
    }

    let size = SIZE { cx: width, cy: height };
    let origin = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    UpdateLayeredWindow(
        hwnd,
        null_mut(),
        null(),
        &size,
        dc.0,
        &origin,
        0,
        &blend,
        ULW_ALPHA,
    );

    (width, height)
}

unsafe fn font(face: &str, height: i32, weight: i32) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = height;
    lf.lfWeight = weight;
    lf.lfQuality = ANTIALIASED_QUALITY as _;
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    CreateFontIndirectW(&lf)
}
