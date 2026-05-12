// Native tray-anchored popover window — the v0.3 daily surface.
//
// A borderless, no-taskbar, always-on-top winit Window with a WebView2 (via
// `wry`) filling it, pointed at http://localhost:17893/popover — the same HTML
// the browser prototype served, unchanged. Created hidden at app start so the
// WebView2 environment + controller are warm before the first Ctrl+Shift+/;
// show/hide just flips visibility + focus (the page keeps its WebSocket open
// while hidden, so it never goes stale). The page dismisses itself by posting
// `"hide"` over the wry IPC channel (its Esc handler), which wakes the main
// loop via a `UserMessage::PopoverHide` to actually hide the window.
//
// Positioning: bottom-right of the primary monitor's work area (SPI_GETWORKAREA,
// so it sits *above* the taskbar). It is not pinned to the exact tray-icon
// rectangle — the `tray-icon` crate doesn't expose the hWnd/uID that
// `Shell_NotifyIcon(NIM_GETRECT)` needs — but the work-area bottom-right corner
// is where the tray clock lives on a standard taskbar, so it reads as "above
// the tray". Multi-monitor: follows the primary monitor (the one with the
// taskbar) regardless of where the mouse is. Known gap (v1): no click-away
// dismiss — a child WebView2 grabbing focus makes the winit parent fire a
// spurious Focused(false), so that path needs a low-level hook, not the obvious
// event. Dismiss is Esc / the Ctrl+Shift+/ toggle / the tray menu for now.

#![cfg(all(windows, feature = "live-view"))]

use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use winit::platform::windows::WindowBuilderExtWindows;
use winit::window::{Window, WindowBuilder, WindowId, WindowLevel};
use windows_sys::Win32::Foundation::HWND;

use super::{live_view, UserMessage};

// The popover window is transparent — only the 360px card (+ its drop shadow)
// is painted; everything else is the desktop showing through. The card wants
// ~360 + a ~12px page frame each side for the shadow, so ~384px. POPOVER_H is
// just the initial height before the page reports its real content height over
// the `resize:` IPC (handled below) — set near a typical running+Today popover
// so opening barely re-snaps. Height is auto-clamped to the work area if it's
// shorter (small laptops) — see `reposition`.
const POPOVER_W: f64 = 384.0;
const POPOVER_H: f64 = 600.0;
const POPOVER_MIN_H: f64 = 240.0;
const EDGE_MARGIN: i32 = 12;

pub struct PopoverWindow {
    window: Window,
    // Held for lifetime; the WebView dies with the window. We poke it via
    // `load_url` when summoned in add-mode.
    webview: wry::WebView,
    visible: bool,
    /// HWND that had foreground focus when we last showed the popover, so we
    /// can hand focus back on hide. Null = nothing captured.
    prev_foreground: HWND,
    /// Logical-px height the page asked for via the `resize:` IPC, so the
    /// window hugs the popover card instead of leaving a grey margin above /
    /// below it. Starts at `POPOVER_H` and shrinks once the page reports its
    /// content height after first paint (and on every re-render).
    content_h: f64,
}

impl PopoverWindow {
    /// Build the hidden popover window and warm up WebView2. Returns `None`
    /// if the window or the WebView2 runtime can't be created (e.g. a very
    /// old Windows without the evergreen runtime) — callers fall back to
    /// opening the popover in the default browser.
    pub fn new(
        elwt: &EventLoopWindowTarget<UserMessage>,
        proxy: EventLoopProxy<UserMessage>,
    ) -> Option<Self> {
        let window = match WindowBuilder::new()
            .with_title("Time Tracker — popover")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(LogicalSize::new(POPOVER_W, POPOVER_H))
            .build(elwt)
        {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "popover window create failed — falling back to browser");
                return None;
            }
        };

        let ipc_proxy = proxy.clone();
        let webview = match wry::WebViewBuilder::new(&window)
            .with_url(format!("http://localhost:{}/popover", live_view::PORT))
            .with_transparent(true)   // page paints only the card; the rest is the desktop
            .with_ipc_handler(move |req: wry::http::Request<String>| {
                let body = req.body().trim();
                match body {
                    "hide" => { let _ = ipc_proxy.send_event(UserMessage::PopoverHide); }
                    "open-recorded" => { let _ = ipc_proxy.send_event(UserMessage::PopoverOpenRecorded); }
                    _ if body.starts_with("resize:") => {
                        if let Ok(h) = body["resize:".len()..].trim().parse::<u32>() {
                            let _ = ipc_proxy.send_event(UserMessage::PopoverResize(h));
                        }
                    }
                    _ => {}
                }
            })
            .build()
        {
            Ok(wv) => wv,
            Err(e) => {
                tracing::warn!(error = %e, "popover WebView2 init failed (runtime missing?) — falling back to browser");
                return None;
            }
        };

        let pw = Self {
            window,
            webview,
            visible: false,
            prev_foreground: std::ptr::null_mut(),
            content_h: POPOVER_H,
        };
        pw.exclude_from_capture();
        pw.reposition();
        tracing::info!("popover window created (hidden, WebView2 warm, capture-excluded)");
        Some(pw)
    }

    /// Client confidentiality: keep this window out of every screen capture —
    /// recordings, screenshots, and the shared screen in Teams / Zoom / Meet.
    /// `WDA_EXCLUDEFROMCAPTURE` (Windows 10 2004+) renders the window invisible
    /// to all capture APIs while it stays fully visible/interactive locally, so
    /// a client's name on the popover can never leak into a shared screen. No
    /// fragile "is a share active?" detection needed — and nothing for the user
    /// to remember to toggle. On pre-2004 Windows the call is a harmless no-op.
    fn exclude_from_capture(&self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        };
        let hwnd = self.window_hwnd();
        if !hwnd.is_null() {
            unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) };
        }
    }

    /// Resize the window to the popover card's actual content height (logical
    /// px, reported by the page over the `resize:` IPC), then re-anchor it
    /// bottom-right. Keeps the card snug against the tray instead of floating
    /// in a tall window with dead grey above and below it.
    pub fn set_content_height(&mut self, h: u32) {
        // No tight upper bound here — `reposition` clamps to the work area, and
        // taller states (the Today tab) scroll their list internally.
        let h = (h as f64).max(POPOVER_MIN_H);
        if (h - self.content_h).abs() < 1.0 {
            return;
        }
        self.content_h = h;
        self.reposition();
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Size + place the window at the bottom-right of the primary monitor's
    /// work area (taskbar-excluded), so it floats just above the tray clock.
    /// Height is clamped to fit the work area so it never runs off the screen.
    fn reposition(&self) {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};

        let scale = self.window.current_monitor().map(|m| m.scale_factor()).unwrap_or(1.0);
        let phys_w = (POPOVER_W * scale).round() as i32;
        let mut phys_h = (self.content_h * scale).round() as i32;
        let min_h = (POPOVER_MIN_H * scale).round() as i32;

        let mut wa = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let have_wa = unsafe {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut RECT as *mut core::ffi::c_void, 0)
        } != 0;
        let (wa_left, wa_top, wa_right, wa_bottom) = if have_wa {
            (wa.left, wa.top, wa.right, wa.bottom)
        } else {
            let sz = self.window.current_monitor().map(|m| m.size()).unwrap_or(PhysicalSize::new(1920, 1080));
            (0, 0, sz.width as i32, sz.height as i32 - 48) // assume a ~48px bottom taskbar
        };

        // clamp height to what the work area can hold (small laptop screens)
        let avail_h = (wa_bottom - wa_top - 2 * EDGE_MARGIN).max(min_h);
        phys_h = phys_h.clamp(min_h, avail_h);
        let _ = self.window.request_inner_size(PhysicalSize::new(phys_w.max(1) as u32, phys_h.max(1) as u32));

        let x = (wa_right - phys_w - EDGE_MARGIN).max(wa_left);
        let y = (wa_bottom - phys_h - EDGE_MARGIN).max(wa_top);
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }

    /// Show + focus the popover. `add_mode` navigates to `/popover?add=1` so
    /// the page boots straight into the add-workstream form with the
    /// "opened from closed → Esc closes the popover" semantics.
    pub fn show(&mut self, add_mode: bool) {
        self.capture_foreground();
        if add_mode {
            let _ = self
                .webview
                .load_url(&format!("http://localhost:{}/popover?add=1", live_view::PORT));
        }
        self.reposition();
        self.window.set_visible(true);
        self.window.focus_window();
        self.visible = true;
    }

    /// Show + focus the popover with the workstream filter open
    /// (Ctrl+Shift+'). `?find=1` boots the page straight into find-mode with
    /// the "opened from closed → Esc closes the popover" semantics.
    pub fn show_find(&mut self) {
        self.capture_foreground();
        let _ = self
            .webview
            .load_url(&format!("http://localhost:{}/popover?find=1", live_view::PORT));
        self.reposition();
        self.window.set_visible(true);
        self.window.focus_window();
        self.visible = true;
    }

    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.window.set_visible(false);
        self.visible = false;
        self.restore_foreground();
    }

    pub fn toggle(&mut self) {
        if self.visible {
            self.hide();
        } else {
            self.show(false);
        }
    }

    fn capture_foreground(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let hwnd = unsafe { GetForegroundWindow() };
        // Don't capture our own window (e.g. an add-mode reload while visible).
        if !hwnd.is_null() && hwnd != self.window_hwnd() {
            self.prev_foreground = hwnd;
        }
    }
    fn restore_foreground(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
        let hwnd = std::mem::replace(&mut self.prev_foreground, std::ptr::null_mut());
        if !hwnd.is_null() {
            // Best-effort — Windows' foreground-lock timeout may decline this,
            // in which case focus just falls to the next window in z-order,
            // which (since we're a no-taskbar tool window) is normally right.
            unsafe {
                SetForegroundWindow(hwnd);
            }
        }
    }

    fn window_hwnd(&self) -> HWND {
        // raw-window-handle 0.5 (the crate's direct dep, same as popup.rs).
        use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
        match self.window.raw_window_handle() {
            RawWindowHandle::Win32(h) => h.hwnd as HWND,
            _ => std::ptr::null_mut(),
        }
    }
}
