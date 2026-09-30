//! Windows backend: one borderless, topmost popup window over the virtual desktop.
//!
//! Decisions:
//!
//! * **One window over the whole virtual desktop, not one per monitor.** The process opts in
//!   to per-monitor DPI awareness v2 before creating it, so window and mouse coordinates are
//!   true physical pixels on every monitor, exactly the virtual-desktop pixels of
//!   `ssx-types`. One window means one buffer, no seams and no per-monitor `WM_DPICHANGED`
//!   resizing; the DPI-dependent part (handle/text/loupe size) is handled by the model's
//!   per-monitor UI scale. `WM_DPICHANGED` is answered by restoring our rectangle.
//! * The window is opaque (not a layered window): the desktop is frozen, so we paint the
//!   dimmed capture ourselves and never need per-pixel alpha or click-through.
//! * Painting is `SetDIBitsToDevice` of each dirty rectangle from a shadow buffer, on the
//!   same thread as message handling.
//! * The window procedure only translates messages into events in a mailbox; the model,
//!   renderer and painting run in the main loop, so nothing re-enters application code from
//!   inside a Win32 callback.
//! * Foreground activation is forced with the classic Alt-tap trick so Esc works even when
//!   the helper was started from a global hotkey handled by another process.
//!
//! This module is compile-checked on Linux (`cargo check --target x86_64-pc-windows-msvc`);
//! the manual test in `tests/windows_manual.rs` exercises it on a real machine.
#![allow(unsafe_code)] // Win32 FFI; every unsafe block below carries a SAFETY comment

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    time::Instant,
};

use ssx_types::{Point, Rect, Size};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, HBRUSH, ReleaseDC,
            SetDIBitsToDevice,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Input::KeyboardAndMouse::{
                GetKeyState, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, ReleaseCapture, SetCapture,
                SetFocus, VK_MENU, keybd_event,
            },
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
                DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HWND_TOPMOST,
                IDC_CROSS, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
                KillTimer, LoadCursorW, MSG, PM_REMOVE, PeekMessageW, RegisterClassExW, SW_SHOW,
                SWP_NOACTIVATE, SWP_SHOWWINDOW, SetCursor, SetForegroundWindow, SetTimer,
                SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, UnregisterClassW,
                WM_CLOSE, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
                WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
                WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
                WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};

use super::{
    Failure,
    win_input::{key_from_vk, modifiers_from_mk, point_from_lparam, wheel_delta},
};
use crate::{
    app::OverlayApp,
    model::{CursorHint, InputEvent, Key, KeyEvent, PointerButton, PointerEvent, geometry::Handle},
    render::TargetBuf,
};

const TIMER_ID: usize = 1;

fn setup(e: impl std::fmt::Display) -> Failure {
    Failure::Setup(e.to_string())
}

/// State shared with the window procedure through `GWLP_USERDATA`. Only ever accessed as a
/// shared reference (interior mutability), so re-entrant messages are sound.
struct Mailbox {
    origin: Point,
    rect: Rect,
    events: RefCell<Vec<InputEvent>>,
    hint: Cell<CursorHint>,
    closed: Cell<bool>,
    started: Instant,
}

impl Mailbox {
    fn push(&self, e: InputEvent) {
        self.events.borrow_mut().push(e);
    }
}

fn cursor_for(hint: CursorHint) -> PCWSTR {
    match hint {
        CursorHint::Crosshair => IDC_CROSS,
        CursorHint::Move => IDC_SIZEALL,
        CursorHint::Resize(h) => match h {
            Handle::NorthWest | Handle::SouthEast => IDC_SIZENWSE,
            Handle::NorthEast | Handle::SouthWest => IDC_SIZENESW,
            Handle::North | Handle::South => IDC_SIZENS,
            Handle::East | Handle::West => IDC_SIZEWE,
        },
    }
}

/// The window procedure.
///
/// # Safety
/// Called by Windows with valid handles; `GWLP_USERDATA` is either 0 or the `Mailbox`
/// pointer installed by `run`, which outlives the window.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: see the function-level contract; a null pointer means "not installed yet".
    let mb = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Mailbox).as_ref() };
    let Some(mb) = mb else {
        // SAFETY: forwarding the message we were called with.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    // SAFETY: `GetKeyState` only reads the calling thread's keyboard state.
    let alt = || unsafe { GetKeyState(i32::from(VK_MENU.0)) } < 0;
    let mods = |w: usize| InputEvent::Modifiers(modifiers_from_mk(w, alt()));
    let pos = |l: isize| point_from_lparam(l, mb.origin);
    let now = || mb.started.elapsed().as_millis() as u64;
    let down = |button: PointerButton| {
        mb.push(mods(wparam.0));
        mb.push(InputEvent::Pointer(PointerEvent::Down {
            pos: pos(lparam.0),
            button,
            time_ms: now(),
        }));
    };
    let up = |button: PointerButton| {
        mb.push(InputEvent::Pointer(PointerEvent::Up { pos: pos(lparam.0), button }));
    };
    match msg {
        WM_MOUSEMOVE => {
            mb.push(mods(wparam.0));
            mb.push(InputEvent::Pointer(PointerEvent::Move { pos: pos(lparam.0) }));
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // SAFETY: valid window handle owned by this thread.
            unsafe { SetCapture(hwnd) };
            down(PointerButton::Left);
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            down(PointerButton::Right);
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            down(PointerButton::Middle);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            up(PointerButton::Left);
            // SAFETY: releasing the capture set above; harmless if not held.
            let _ = unsafe { ReleaseCapture() };
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            up(PointerButton::Right);
            LRESULT(0)
        }
        WM_MBUTTONUP => {
            up(PointerButton::Middle);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            mb.push(InputEvent::Pointer(PointerEvent::Wheel { delta: wheel_delta(wparam.0) }));
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let key = key_from_vk(wparam.0 as u32);
            if key != Key::Other {
                mb.push(InputEvent::Key(KeyEvent { key, pressed }));
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // SAFETY: loading a stock cursor and selecting it on this thread.
            unsafe {
                if let Ok(c) = LoadCursorW(None, cursor_for(mb.hint.get())) {
                    SetCursor(Some(c));
                }
            }
            LRESULT(1)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DPICHANGED => {
            // Keep our physical rectangle; the shared desktop is frozen, so rescaling the
            // window would misalign it with the capture.
            let r = mb.rect;
            // SAFETY: valid window handle; plain geometry call.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    r.x,
                    r.y,
                    r.width as i32,
                    r.height as i32,
                    SWP_NOACTIVATE,
                )
            };
            LRESULT(0)
        }
        WM_CLOSE => {
            mb.closed.set(true);
            LRESULT(0)
        }
        WM_TIMER => LRESULT(0),
        // SAFETY: forwarding the message we were called with.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Sends the dirty rectangle `r` (desktop pixels) of the shadow buffer to the window.
fn paint(hwnd: HWND, buf: &[u8], origin: Point, size: Size, r: Rect) {
    let bounds = Rect::from_origin_size(origin, size);
    let Some(r) = r.intersect(bounds) else { return };
    let stride = size.width as usize * 4;
    let row_bytes = r.width as usize * 4;
    let (lx, ly) = ((r.x - origin.x) as usize, (r.y - origin.y) as usize);
    // Copy the rectangle into a tight top-down buffer: avoids the bottom-up `ySrc` quirk
    // of top-down DIBs entirely.
    let mut tight = Vec::with_capacity(row_bytes * r.height as usize);
    for row in 0..r.height as usize {
        let o = (ly + row) * stride + lx * 4;
        tight.extend_from_slice(&buf[o..o + row_bytes]);
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: r.width as i32,
            biHeight: -(r.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `hwnd` is our live window; the DC is released before returning; `tight`
    // holds exactly `r.width * r.height` BGRA pixels as described by `bmi`.
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.is_invalid() {
            return;
        }
        SetDIBitsToDevice(
            hdc,
            r.x - origin.x,
            r.y - origin.y,
            r.width,
            r.height,
            0,
            0,
            0,
            r.height,
            tight.as_ptr().cast::<c_void>(),
            &raw const bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(Some(hwnd), hdc);
    }
}

/// Runs the overlay on the desktop the process is attached to.
pub(super) fn run(app: &mut OverlayApp) -> Result<(), Failure> {
    let bounds = app.bounds();
    let size = Size::new(bounds.width, bounds.height);
    let origin = bounds.origin();

    // SAFETY: plain process-wide setting; failure (already set by a manifest) is fine.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    // SAFETY: querying our own module handle.
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }.map_err(setup)?.into();
    let class = w!("ssx-overlay");
    // SAFETY: loading a stock cursor; the class struct is fully initialised and outlives
    // the call.
    let atom = unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&raw const wc)
    };
    if atom == 0 {
        return Err(setup("RegisterClassExW failed"));
    }

    let mailbox = Mailbox {
        origin,
        rect: bounds,
        events: RefCell::new(Vec::new()),
        hint: Cell::new(CursorHint::Crosshair),
        closed: Cell::new(false),
        started: Instant::now(),
    };

    // SAFETY: creating a top-level popup on this thread with valid class/instance.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("ssx-overlay"),
            WS_POPUP,
            bounds.x,
            bounds.y,
            bounds.width as i32,
            bounds.height as i32,
            None,
            None,
            Some(hinst),
            None,
        )
    }
    .map_err(|e| {
        // SAFETY: unregistering the class registered above.
        let _ = unsafe { UnregisterClassW(class, Some(hinst)) };
        setup(format!("CreateWindowExW failed: {e}"))
    })?;
    // SAFETY: `mailbox` lives until after the window is destroyed below, and the window
    // procedure only ever takes a shared reference to it.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, std::ptr::addr_of!(mailbox) as isize) };

    let result = message_loop(app, &mailbox, hwnd, origin, size);

    // SAFETY: tear down what we created, in reverse order; the userdata pointer is cleared
    // first so a late message cannot see a dangling mailbox.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = KillTimer(Some(hwnd), TIMER_ID);
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(class, Some(hinst));
    }
    result
}

fn message_loop(
    app: &mut OverlayApp,
    mb: &Mailbox,
    hwnd: HWND,
    origin: Point,
    size: Size,
) -> Result<(), Failure> {
    let mut buf = vec![0u8; size.width as usize * size.height as usize * 4];
    // SAFETY: showing and activating our own window; the Alt tap lets a background process
    // take the foreground (see the module docs).
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            origin.x,
            origin.y,
            size.width as i32,
            size.height as i32,
            SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS::default(), 0);
        keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        SetTimer(Some(hwnd), TIMER_ID, 200, None);
    }

    let mut first = true;
    let mut need_frame = true;
    loop {
        if need_frame {
            let rects = app.begin_frame();
            for r in &rects {
                {
                    let mut t = TargetBuf { origin, size, data: &mut buf };
                    app.render(*r, &mut t);
                }
                paint(hwnd, &buf, origin, size, *r);
            }
            mb.hint.set(app.cursor_hint());
            if first {
                app.mark_first_frame();
                first = false;
            }
            need_frame = false;
        }
        if mb.closed.get() {
            app.cancel();
        }
        if app.outcome().is_some() {
            return Ok(());
        }
        if let Some(d) = app.deadline()
            && Instant::now() >= d
        {
            app.cancel();
            return Ok(());
        }

        let mut msg = MSG::default();
        // SAFETY: standard message pump on the thread that owns the window.
        unsafe {
            if !GetMessageW(&raw mut msg, None, 0, 0).as_bool() {
                app.cancel();
                return Ok(());
            }
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
            // Drain what is already queued so a burst of mouse moves renders once.
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        let evs: Vec<InputEvent> = mb.events.borrow_mut().drain(..).collect();
        if !evs.is_empty() {
            need_frame = true;
            for e in evs {
                app.handle(e);
            }
        }
    }
}
