//! Draws the taskbar widget into a per-pixel-alpha 32-bpp DIB and pushes it with `UpdateLayeredWindow`,
//! so icons and text blend cleanly over the translucent Windows 11 taskbar.

use crate::audio::{AudioSnapshot, DeviceKind};
use crate::i18n::t;
use crate::wide;
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
    let sub_fg = if light { 0x333333 } else { 0xD0D0D0 };
    let muted_red = if light { 0xC42B1C } else { 0xFF6B6B };
    // Vibrant emerald/amber accent when in communication mode (Teams / Discord / active mic)
    let comm_color = if light { 0x0F7B0F } else { 0x50E3C2 };

    let mut segs = Vec::new();
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

    let main_color = if snap.master_muted {
        muted_red
    } else if snap.in_communication {
        comm_color
    } else {
        fg
    };
    let main_opacity = if snap.master_muted { 195 } else { 255 };

    // 1. Device / volume icon
    let icon_glyph = device_icon(snap.device_kind, snap.master_volume, snap.master_muted);
    segs.push(seg(icon_glyph, true, main_color, main_opacity, s(5)));

    // 2. Master volume percentage (or Muted label)
    let vol_text = if snap.master_muted {
        t().muted.to_string()
    } else {
        format!("{}%", snap.master_volume)
    };
    let has_extra = (show_device_label && !snap.device_short_name.is_empty()) || snap.in_communication;
    segs.push(seg(
        &vol_text,
        false,
        if snap.master_muted { muted_red } else { fg },
        main_opacity,
        if has_extra { s(6) } else { 0 },
    ));

    // 3. Optional short device name on horizontal taskbars
    if matches!(layout, Layout::Row { .. }) && show_device_label && !snap.device_short_name.is_empty() {
        let gap = if snap.in_communication { s(6) } else { 0 };
        segs.push(seg(&snap.device_short_name, false, sub_fg, 220, gap));
    }

    // 4. Communication mode indicator (Teams / Discord / Call active)
    if snap.in_communication {
        segs.push(seg(ICON_COMM_MIC, true, comm_color, 255, 0));
    }

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    let text_font = font("Segoe UI", -s(12), FW_SEMIBOLD as i32);
    let icon_font = font("Segoe Fluent Icons", -s(15), FW_NORMAL as i32);

    // Measure segments
    for item in &mut segs {
        SelectObject(dc, if item.icon { icon_font } else { text_font });
        let mut size: SIZE = zeroed();
        GetTextExtentPoint32W(dc, item.text.as_ptr(), item.text.len() as i32 - 1, &mut size);
        item.w = size.cx;
        item.h = size.cy;
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
    let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteObject(text_font);
        DeleteObject(icon_font);
        DeleteDC(dc);
        return (width, height);
    }
    let old_bmp = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as _);
    SetTextColor(dc, 0x00FF_FFFF);
    for item in &segs {
        SelectObject(dc, if item.icon { icon_font } else { text_font });
        TextOutW(
            dc,
            item.x,
            item.y,
            item.text.as_ptr(),
            item.text.len() as i32 - 1,
        );
    }
    GdiFlush();

    let mut paint = vec![(fg, 255u32); (width * height) as usize];
    for item in &segs {
        let (x0, x1) = (item.x.max(0), (item.x + item.w).min(width));
        let (y0, y1) = (item.y.max(0), (item.y + item.h).min(height));
        for y in y0..y1 {
            for x in x0..x1 {
                paint[(y * width + x) as usize] = (item.color, item.opacity);
            }
        }
    }
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
    for (i, p) in px.iter_mut().enumerate() {
        let (color, opacity) = paint[i];
        let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
        let a = cov * opacity / 255;
        // Alpha 1 keeps the whole bounding box clickable/scrollable while staying invisible
        *p = if a == 0 {
            0x0100_0000
        } else {
            let ch = |shift: u32| ((color >> shift) & 0xFF) * a / 255;
            (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
        };
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
        dc,
        &origin,
        0,
        &blend,
        ULW_ALPHA,
    );

    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteObject(text_font);
    DeleteObject(icon_font);
    DeleteDC(dc);
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
