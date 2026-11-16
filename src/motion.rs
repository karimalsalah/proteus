//! Reduced motion. Windows' "Animation effects" switch (Settings >
//! Accessibility > Visual effects; SPI_GETCLIENTAREAANIMATION) is read when
//! the window opens and again whenever the system says a setting changed.
//! With animations off nothing on the grid moves: text appears whole, the
//! light field stays dark, the cursor neither glides nor flares, the World
//! view places what arrives without unfolding it (`Renderer::set_still`).
//! A change is drawn once, and no frame follows it.
//!
//! WM_SETTINGCHANGE is sent to top-level windows, not posted to the thread's
//! queue, so the window library never shows it to us; the window procedure
//! is subclassed to see it. The subclass only reads the setting, stores it,
//! and asks for a repaint if it changed.
//!
//! `PROTEUS_MOTION=still|full` overrides the system setting (tests, and a
//! person who wants the terminal still without changing all of Windows).
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, WM_SETTINGCHANGE};

static STILL: AtomicBool = AtomicBool::new(false);
/// Settings-changed messages seen by the subclass (for tests and the log).
static SEEN: AtomicU64 = AtomicU64::new(0);
const SUBCLASS_ID: usize = 0x5052_4f54; // "PROT"

/// What the system (or the override) says right now: true when animations are off.
pub fn system_still() -> bool {
    match std::env::var("PROTEUS_MOTION").as_deref() {
        Ok("still") => return true,
        Ok("full") => return false,
        _ => {}
    }
    let mut on: i32 = 1;
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &mut on as *mut i32 as *mut core::ffi::c_void, 0) } != 0;
    ok && on == 0
}

/// The setting as last read.
pub fn still() -> bool {
    STILL.load(Ordering::Acquire)
}

/// Read the setting now and follow it from here on. Call on the window's
/// own thread. Returns false if the window could not be watched (the
/// setting is then read once, at start).
pub fn watch(hwnd: isize) -> bool {
    STILL.store(system_still(), Ordering::Release);
    unsafe { SetWindowSubclass(hwnd as HWND, Some(subclass), SUBCLASS_ID, 0) != 0 }
}

unsafe extern "system" fn subclass(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_SETTINGCHANGE {
        SEEN.fetch_add(1, Ordering::Relaxed);
        let now = system_still();
        if STILL.swap(now, Ordering::AcqRel) != now {
            // The window draws on the next paint and picks the new value up there.
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, SendMessageW, SPI_SETCLIENTAREAANIMATION, WS_OVERLAPPED};

    /// A real window procedure, subclassed the way the terminal's is, sees
    /// the settings message and re-reads the setting. The window is never
    /// shown, and the system setting itself is never changed: the message is
    /// sent to this one window only.
    #[test]
    fn a_settings_message_is_seen_and_the_setting_read_again() {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_OVERLAPPED, 0, 0, 10, 10, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
        assert!(!hwnd.is_null(), "could not create a hidden test window");
        assert!(watch(hwnd as isize), "the window procedure could not be subclassed");
        let truth = system_still();
        assert_eq!(still(), truth);
        // Pretend the last reading was wrong; the message must correct it.
        STILL.store(!truth, Ordering::Release);
        let before = SEEN.load(Ordering::Relaxed);
        unsafe { SendMessageW(hwnd, WM_SETTINGCHANGE, SPI_SETCLIENTAREAANIMATION as usize, 0) };
        assert_eq!(SEEN.load(Ordering::Relaxed), before + 1, "the subclass did not see the message");
        assert_eq!(still(), truth, "the setting was not read again");
        unsafe { DestroyWindow(hwnd) };
        println!("motion: system animations {} (SPI_GETCLIENTAREAANIMATION)", if truth { "off" } else { "on" });
    }
}
