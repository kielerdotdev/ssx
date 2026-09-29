//! Small shared helpers for the Windows glue: error conversion, COM/`WinRT` initialisation
//! and raw-handle round-trips.

use std::ffi::c_void;

use windows::Win32::{
    Foundation::{HWND, RPC_E_CHANGED_MODE},
    Graphics::Gdi::HMONITOR,
    System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

use crate::error::WinError;

/// Converts a `windows` error into a [`WinError`] tagged with what was being attempted.
pub(crate) fn api_err(context: &'static str) -> impl FnOnce(windows::core::Error) -> WinError {
    move |e| WinError::api(context, e.code().0, e.message())
}

/// Turns the raw integer carried in [`crate::chain::MonitorTarget`] back into an `HMONITOR`.
pub(crate) fn hmonitor(raw: isize) -> HMONITOR {
    HMONITOR(raw as *mut c_void)
}

/// Turns a raw window id back into an `HWND`.
pub(crate) fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// Keeps the calling thread's WinRT/COM apartment initialised for the thread's lifetime.
struct ComGuard {
    /// `true` if this guard's `RoInitialize` succeeded and must be balanced.
    initialised: bool,
}

impl ComGuard {
    fn init() -> Self {
        // SAFETY: `RoInitialize` has no preconditions; it is balanced by `RoUninitialize`
        // in `Drop` on the same thread (thread-local destructors run on the owning thread).
        let initialised = match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => true,
            // The thread is already a single-threaded apartment (e.g. a UI thread). WinRT
            // works there too, and we must not uninitialise what we did not initialise.
            Err(e) if e.code() == RPC_E_CHANGED_MODE => false,
            Err(e) => {
                tracing::warn!(error = %e, "RoInitialize failed; WinRT calls may fail");
                false
            }
        };
        Self { initialised }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialised {
            // SAFETY: balances the successful `RoInitialize` in `init` on this thread.
            unsafe { RoUninitialize() };
        }
    }
}

thread_local! {
    static COM: ComGuard = ComGuard::init();
}

/// Makes sure the calling thread can use `WinRT` / COM. Cheap after the first call.
pub(crate) fn ensure_com() {
    // Accessing the thread-local runs `ComGuard::init` on first use. During thread
    // teardown `try_with` fails; nothing useful can be done then.
    let _ = COM.try_with(|_| ());
}
