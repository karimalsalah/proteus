//! Following the default output device. Windows tells registered programs
//! when the default output changes (headphones plugged in, a dock, a
//! choice in Settings) through IMMNotificationClient. The callback arrives
//! on a thread of the audio service's and must not block or call back into
//! the audio API, so it does almost nothing: it sets a flag, and on the
//! flag's rising edge (the first callback of a burst; Windows sends one per
//! role) it nudges the sound thread with one message that cannot block. The
//! sound thread clears the flag and opens the new default, paused, so the
//! next sound plays there. Nothing polls: with no change, nothing runs.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use windows::Win32::Media::Audio::{eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows_core::{implement, PCWSTR};

/// Microseconds on this process's own clock.
pub fn now_us() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// What the callback leaves for the sound thread.
#[derive(Default)]
pub struct Changed {
    pub flag: AtomicBool,
    /// When the first callback of the current burst came (`now_us`).
    pub at_us: AtomicU64,
    /// Callbacks about the default output, all roles that matter, ever.
    pub callbacks: AtomicU64,
}

#[implement(IMMNotificationClient)]
struct Client {
    changed: Arc<Changed>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl IMMNotificationClient_Impl for Client_Impl {
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows_core::Result<()> {
        // Output only, and the roles a program's sound plays in (cpal opens
        // the console default; Settings moves multimedia with it).
        if flow == eRender && (role == eConsole || role == eMultimedia) {
            self.changed.callbacks.fetch_add(1, Ordering::Relaxed);
            if !self.changed.flag.swap(true, Ordering::AcqRel) {
                self.changed.at_us.store(now_us(), Ordering::Release);
                (self.wake)();
            }
        }
        Ok(())
    }
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows_core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows_core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows_core::Result<()> {
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows_core::Result<()> {
        Ok(())
    }
}

/// A registration, held by the sound thread for its whole life.
pub struct Watch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    pub changed: Arc<Changed>,
}

/// The last client registered in this process, as a raw interface pointer,
/// so a test can call it exactly as the audio service does.
#[cfg(test)]
pub static LAST: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

impl Watch {
    /// Register from the calling thread (COM is initialised here, free-threaded).
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        unsafe {
            // S_FALSE (already initialised) is fine; a different mode is not.
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("COM: {e}"))?;
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("device enumerator: {e}"))?;
            let changed = Arc::new(Changed::default());
            let client: IMMNotificationClient = Client { changed: changed.clone(), wake: Box::new(wake) }.into();
            enumerator.RegisterEndpointNotificationCallback(&client).map_err(|e| format!("register: {e}"))?;
            #[cfg(test)]
            {
                if let Ok(mut l) = LAST.lock() {
                    *l = windows_core::Interface::as_raw(&client) as usize;
                }
            }
            Ok(Self { enumerator, client, changed })
        }
    }

    /// The id of the default output now (the role cpal opens: console).
    /// Asked on the sound thread, never in the callback.
    pub fn default_id(&self) -> Option<String> {
        unsafe {
            let dev = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let p = dev.GetId().ok()?;
            let s = p.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
            s
        }
    }

    /// Take the flag: Some(when the change was first reported) if it was set.
    pub fn take(&self) -> Option<u64> {
        self.changed.flag.swap(false, Ordering::AcqRel).then(|| self.changed.at_us.load(Ordering::Acquire))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.enumerator.UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}
