//! Lightweight RAII wrappers for Win32 GDI, COM, and WinHTTP handles so no early return can leak resources.

use std::ffi::c_void;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    COINIT_MULTITHREADED,
};
use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, SelectObject, HDC, HGDIOBJ};

/// Initializes COM on the current thread and calls `CoUninitialize` on drop when initialization succeeded.
pub struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    pub fn init_sta() -> Self {
        Self::init(COINIT(COINIT_APARTMENTTHREADED.0 | COINIT_DISABLE_OLE1DDE.0))
    }

    pub fn init_mta() -> Self {
        Self::init(COINIT_MULTITHREADED)
    }

    fn init(flags: COINIT) -> Self {
        let hr = unsafe { CoInitializeEx(None, flags) };
        Self { uninit: hr.is_ok() }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { CoUninitialize() };
        }
    }
}

/// Owned memory `HDC` created via `CreateCompatibleDC`; deleted on drop.
pub struct OwnedDc(pub HDC);

impl Drop for OwnedDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteDC(self.0) };
        }
    }
}

/// Owned GDI object (`HFONT`, `HBITMAP`, `HBRUSH`, `HPEN`); deleted on drop.
/// Must be declared *before* any `SelectedObj` that selects it into an `HDC` so that
/// Rust's reverse drop order restores the previous object before calling `DeleteObject`.
pub struct OwnedGdi(pub HGDIOBJ);

impl Drop for OwnedGdi {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

/// Temporarily selects a GDI object into an `HDC` and restores the previous object on drop.
pub struct SelectedObj {
    dc: HDC,
    prev: HGDIOBJ,
}

impl SelectedObj {
    pub unsafe fn select(dc: HDC, obj: HGDIOBJ) -> Self {
        let prev = SelectObject(dc, obj);
        Self { dc, prev }
    }
}

impl Drop for SelectedObj {
    fn drop(&mut self) {
        if !self.dc.is_null() && !self.prev.is_null() {
            unsafe { SelectObject(self.dc, self.prev) };
        }
    }
}

/// Owned WinHTTP handle (`HINTERNET`); closed with `WinHttpCloseHandle` on drop.
pub struct OwnedWinHttp(pub *mut c_void);

impl Drop for OwnedWinHttp {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
            }
        }
    }
}
