// SPEC §7 entry point - winit event loop owning the popup window plus
// channel-based polling for tray + hotkey + IPC.
//
// Architecture (see src/popup.rs for the GL/egui rendering details):
//
//   winit::EventLoop  (this thread)
//     ├── Event::WindowEvent { id == popup.window_id() }  -> popup.on_window_event()
//     ├── Event::WindowEvent { event: RedrawRequested }   -> popup.paint()
//     ├── Event::UserEvent(UserMessage)                   -> popup.show()
//     └── ControlFlow::Wait                               (sleep until input)
//
//   Polled each iteration (channel-driven, cross-thread):
//     ├── GlobalHotKeyEvent::receiver()                   -> popup.show()
//     ├── MenuEvent::receiver()                           -> popup.show() / Quit
//     └── (pipe server thread sends UserEvent via proxy)  -> popup.show()
//
//   Background threads:
//     ├── single_instance pipe server  (named pipe, posts UserEvent via proxy)
//     └── csv_writer writer thread     (mpsc channel; blocking submit)
//
// Per project memory `build-on-primitives`: this is the rocket-ship
// path - no eframe, no tauri, no electron. Each layer (winit, glutin,
// glow, egui_glow, egui-winit, tray-icon, global-hotkey, windows-sys)
// is wired together by hand so future iteration can reach into any
// layer without fighting a wrapper.

// Pure-logic and Win32-cfg-gated modules live in the lib crate (src/lib.rs)
// so `cargo test --lib --target x86_64-unknown-linux-gnu` runs on Linux
// without dragging in the GUI stack. Only `popup` stays bin-only because
// it owns the manual winit/glutin/glow/egui_glow rendering.
use time_tracker::{
    autocomplete, config, crash, csv_writer, logging, paths, single_instance, timer, usage,
    workstream,
};

// popup.rs lives at src/popup.rs; this module is loaded via #[path] from
// main.rs so its default submodule directory is src/windows_main/. Override.
#[path = "popup.rs"]
mod popup;

// Optional localhost HTTP+WebSocket "live view" — compiled only with the
// `live-view` Cargo feature. See src/live_view.rs.
#[cfg(feature = "live-view")]
#[path = "live_view.rs"]
mod live_view;

// v0.3: native tray-anchored popover window (WebView2 via wry). Same feature
// gate — the popover HTML is served by the live-view server. See
// src/popover_window.rs.
#[cfg(feature = "live-view")]
#[path = "popover_window.rs"]
mod popover_window;

use std::sync::Arc;

use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder};

use self::popup::PopupMode;

const APP_NAME: &str = "Time Tracker";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
enum UserMessage {
    ShowQuickEntry,
    ShowTimerStart,
    ShowTimerStop,
    /// SPEC §3.2: 30s timer-tick persistence wake. Posted by a
    /// dedicated sleep thread (see `spawn_timer_tick_thread`). Not a
    /// popup-show mode.
    TimerTick,
    /// v0.3: timer commands from the popover (POST /timer/start|stop|switch).
    /// The HTTP handler validates the workstream id against the registry,
    /// then posts one of these so the actual timer mutation + CSV write +
    /// broadcast happens on the main thread that owns the Timer. The popup
    /// window is *not* shown — these are headless drives of the same state
    /// the hotkeys touch.
    #[cfg(feature = "live-view")]
    TimerCmdStart { client: String, engagement: String, billable: bool },
    #[cfg(feature = "live-view")]
    TimerCmdStop,
    #[cfg(feature = "live-view")]
    TimerCmdSwitch { client: String, engagement: String, billable: bool },
    /// v0.3: the popover page asked to dismiss itself (its Esc handler posts
    /// `"hide"` over the wry IPC channel). Handled by hiding the native window.
    #[cfg(feature = "live-view")]
    PopoverHide,
    /// v0.3: the popover's "Open Recorded Time" link — open the full app in the
    /// default browser (not inside the popover's own webview) and hide the popover.
    #[cfg(feature = "live-view")]
    PopoverOpenRecorded,
    /// v0.3: the popover page measured its content height (logical px) after a
    /// render and wants the native window resized to match, so the card hugs the
    /// tray with no dead grey margin.
    #[cfg(feature = "live-view")]
    PopoverResize(u32),
}

/// Fan-out handle for the optional live-view dashboard. Wraps the
/// `broadcast::Sender` the live-view server hands back; every `emit*`
/// call is a sync, non-blocking send that drops to a no-op when the
/// `live-view` feature is off or no browser is connected. Cheap to
/// `Clone` — each handler (popup, hotkey paths) keeps its own.
#[derive(Clone, Default)]
pub struct LiveBus {
    #[cfg(feature = "live-view")]
    tx: Option<tokio::sync::broadcast::Sender<String>>,
}

impl LiveBus {
    #[cfg(feature = "live-view")]
    fn connected(tx: tokio::sync::broadcast::Sender<String>) -> Self {
        Self { tx: Some(tx) }
    }

    fn send(&self, _json: String) {
        #[cfg(feature = "live-view")]
        if let Some(tx) = &self.tx {
            // Err just means "no browser listening right now" — fine.
            let _ = tx.send(_json);
        }
    }

    pub fn hotkey(&self, which: &str) {
        self.send(
            serde_json::json!({ "event": "hotkey", "which": which, "at": now_iso() }).to_string(),
        );
    }

    pub fn timer_started(&self, client: &str, engagement: &str, narrative: &str) {
        self.send(
            serde_json::json!({
                "event": "timer_started",
                "client": client,
                "engagement": engagement,
                "narrative": narrative,
                "at": now_iso(),
            })
            .to_string(),
        );
    }

    pub fn timer_stopped(&self, client: &str, minutes: i64) {
        self.send(
            serde_json::json!({
                "event": "timer_stopped",
                "client": client,
                "minutes": minutes,
                "at": now_iso(),
            })
            .to_string(),
        );
    }

    /// v0.3: emitted on a timer *switch* (stop-then-start in one move),
    /// after the underlying `timer_stopped` / `entry_logged` / `timer_started`
    /// trio. Carries the engagement we switched away from so the popover can
    /// render the "switched from {prev}" trailer without bookkeeping of its own.
    #[cfg(feature = "live-view")]
    pub fn workstream_switched(&self, from_engagement: &str, to_client: &str, to_engagement: &str) {
        self.send(
            serde_json::json!({
                "event": "workstream_switched",
                "from_engagement": from_engagement,
                "client": to_client,
                "engagement": to_engagement,
                "at": now_iso(),
            })
            .to_string(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn entry_logged(
        &self,
        timestamp_iso: &str,
        staff: &str,
        client: &str,
        engagement: &str,
        narrative: &str,
        minutes: i64,
        billable: bool,
        method: &str,
    ) {
        self.send(
            serde_json::json!({
                "event": "entry_logged",
                "timestamp": timestamp_iso,
                "staff": staff,
                "client": client,
                "engagement": engagement,
                "narrative": narrative,
                "minutes": minutes,
                "hours": (minutes as f64) / 60.0,
                "billable": billable,
                "method": method,
            })
            .to_string(),
        );
    }
}

#[allow(dead_code)] // used only by LiveBus emitters (no-op build still references it indirectly)
fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

#[cfg(feature = "live-view")]
fn open_url_in_browser(url: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Err(e) = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        tracing::warn!(error = %e, url, "failed to open url in default browser");
    }
}

#[cfg(feature = "live-view")]
fn open_recorded_in_browser() {
    open_url_in_browser(&format!("http://localhost:{}/recorded", live_view::PORT));
}

// Fallback path, used only when the native popover window (popover_window.rs)
// couldn't be created — e.g. no WebView2 runtime. Opens /popover in the
// default browser. `add=1` expands the add-workstream form on load.
#[cfg(feature = "live-view")]
fn open_popover_browser() {
    open_popover_browser_inner(false)
}
#[cfg(feature = "live-view")]
fn open_popover_browser_add() {
    open_popover_browser_inner(true)
}
#[cfg(feature = "live-view")]
fn open_popover_browser_inner(add_mode: bool) {
    let url = if add_mode {
        format!("http://localhost:{}/popover?add=1", live_view::PORT)
    } else {
        format!("http://localhost:{}/popover", live_view::PORT)
    };
    open_url_in_browser(&url);
}

pub fn run() {
    if let Err(e) = paths::ensure_layout() {
        eprintln!(
            "FATAL: failed to create data layout at {:?}: {e}",
            paths::data_dir()
        );
        std::process::exit(1);
    }
    let _log_guard = logging::init();
    crash::install_handler();
    tracing::info!(
        version = APP_VERSION,
        data_dir = %paths::data_dir().display(),
        csv_dir = %paths::csv_dir().display(),
        "{APP_NAME} starting"
    );

    // R1: surface a previous-run crash to the user (MessageBox + mailto, no
    // auto-upload), then archive the artifact so it doesn't re-prompt.
    if let Some(crash_path) = crash::find_recent_crash() {
        tracing::warn!(path = %crash_path.display(), "previous run crashed — surfacing to user");
        eprintln!("[recovered] previous run crashed: {}", crash_path.display());
    }
    crash::surface_recent_crash();
    // R1: SPEC §4.1 first-launch self-test (write-and-readback in csv_dir),
    // shown once via a one-line banner.
    crash::first_launch_self_test();

    install_ctrlc_handler();

    // Config: identity (staff name) + defaults + [hotkeys] rebinds + [startup].
    // First-launch writes a discoverable default file.
    let cfg = config::Config::load();
    tracing::info!(staff = %cfg.identity.staff, "config loaded");
    if !cfg.startup.enabled {
        // R2: honoured to the extent the runtime allows — the MSIX
        // `uap5:StartupTask` is a request Windows gates behind a Settings →
        // Startup-Apps consent; the WinRT `StartupTask` API (to disable it
        // from the app) is a v1.1 item. So: don't arm any future
        // app-side autostart step, and tell the user where the real toggle is.
        tracing::info!(
            "config [startup] enabled = false — app will not arm autostart; \
             the OS toggle is Settings → Apps → Startup"
        );
    }

    // Usage instrumentation (per SPEC §5.4). Local JSONL only - no
    // network. Wrapped in Arc so the popup can hold its own handle.
    let usage = Arc::new(usage::Usage::open());
    usage.app_start();
    crash::set_usage(usage.clone());

    // Single-instance gate. If we're the second instance, notify and exit
    // BEFORE creating any windows so we never flash UI.
    let _mutex_guard = match single_instance::acquire_or_notify("show_quick_entry") {
        single_instance::InstanceCheck::Second => {
            tracing::info!("Already running. Notified existing instance and exiting.");
            eprintln!("Already running. Notified existing instance and exiting.");
            std::process::exit(0);
        }
        single_instance::InstanceCheck::First(g) => g,
    };

    // Event loop with our cross-thread UserMessage channel for the IPC
    // pipe server.
    let event_loop = EventLoopBuilder::<UserMessage>::with_user_event()
        .build()
        .expect("event loop");
    let proxy = event_loop.create_proxy();

    let pipe_proxy = proxy.clone();
    single_instance::start_pipe_server(move |msg| {
        let event = match msg.as_str() {
            "show_quick_entry" => Some(UserMessage::ShowQuickEntry),
            "show_timer_start" => Some(UserMessage::ShowTimerStart),
            "show_timer_stop" => Some(UserMessage::ShowTimerStop),
            other => {
                tracing::warn!("[pipe] unknown message: {other:?}");
                None
            }
        };
        if let Some(e) = event {
            let _ = pipe_proxy.send_event(e);
        }
    });

    // CSV writer thread + autocomplete cache (rebuilt from existing CSVs).
    // Writer thread holds its own Arc<Usage> handle so it can emit
    // `queue_drain` events directly without bouncing through the main
    // loop (per SPEC §5.4 wedge instrumentation).
    let csv_writer = csv_writer::spawn(usage.clone());
    let autocomplete = autocomplete::rebuild_from_csvs().unwrap_or_default();

    // v0.2: workstream registry — loaded once (synthesized from existing
    // monthly CSVs on first run after upgrade), held behind a mutex
    // shared with the localhost server. Single-writer: this app.
    let workstreams = Arc::new(std::sync::Mutex::new(workstream::WorkstreamRegistry::load()));

    // Restore any timer that was running across an app restart.
    let timer = timer::Timer::load();
    if timer.is_running() {
        tracing::info!("timer was running across restart - state restored");
    }

    // Optional localhost server (live-view feature): the v0.2 review
    // surface (Recorded Time at `/`, Export, Exports history) plus the
    // popover at `/popover` and the `/workstreams` API. Without the
    // feature this compiles out and `live_bus` is an inert no-op handle.
    // Bind failure is logged and non-fatal (see live_view::start).
    #[cfg(feature = "live-view")]
    let live_bus = LiveBus::connected(live_view::start(
        workstreams.clone(),
        proxy.clone(),
        csv_writer.clone(),
    ));
    #[cfg(not(feature = "live-view"))]
    let live_bus = LiveBus::default();

    // Pre-warmed popup window (created hidden).
    let mut popup = popup::Popup::new(
        &event_loop,
        csv_writer,
        autocomplete,
        timer,
        cfg.identity.staff.clone(),
        usage.clone(),
        live_bus.clone(),
        workstreams.clone(),
    );

    // v0.3: native tray-anchored popover window (WebView2 via wry). Created
    // hidden so the runtime is warm before the first Ctrl+Shift+/. `None` means
    // the window/runtime couldn't be created — the popover hotkeys/menu fall
    // back to opening /popover in the default browser. Only with the live-view
    // server (it hosts the popover HTML).
    #[cfg(feature = "live-view")]
    let mut popover_window = popover_window::PopoverWindow::new(&event_loop, proxy.clone());
    #[cfg(feature = "live-view")]
    let popover_window_id = popover_window.as_ref().map(|pw| pw.window_id());

    // Hotkeys.
    let hotkey_manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "GlobalHotKeyManager::new failed");
            eprintln!("FATAL: GlobalHotKeyManager::new failed: {e}");
            std::process::exit(1);
        }
    };
    let HotkeyRegistration { ids: hotkey_ids, failures: hotkey_failures, outcomes: hotkey_outcomes } =
        register_hotkeys(&hotkey_manager, &cfg.hotkeys);
    // R5: emit one `hotkey_registered { which, ok }` per hotkey at startup —
    // a registration *outcome*, NOT a keypress. `hotkey_fire` is reserved for
    // actual presses (emitted from the event loop below), so the wedge data
    // ("your team fired N hotkeys last week") isn't inflated by phantom events.
    for (which, ok) in &hotkey_outcomes {
        usage.hotkey_registered(which, *ok);
    }
    if !hotkey_failures.is_empty() {
        // R2: surface hotkey conflicts via MessageBoxW once on first launch.
        // Zero-noise when registration succeeds; clear actionable banner
        // when one or more conflict with another app (Logitech Options,
        // AHK, Excel, etc.). Body points at config.toml for rebinding.
        show_hotkey_conflict_dialog(&hotkey_failures);
    }

    // Tray icon + menu.
    let menu = Menu::new();
    let header = MenuItem::new(format!("{APP_NAME} v{APP_VERSION}"), false, None);
    #[cfg(feature = "live-view")]
    let add_workstream_menu = MenuItem::new("Add workstream  (Ctrl+Shift+H)", true, None);
    let timer_start_menu = MenuItem::new("Start timer…", true, None);
    let timer_stop_menu = MenuItem::new("Stop timer  (Ctrl+Shift+' or ;)", true, None);
    // v0.2: "quick entry" lost its hotkey (Ctrl+Shift+H now adds a
    // workstream); kept here as "Log a block…" until Recorded Time's
    // write path lands. In a tray-only build (no live-view) it stays the
    // Ctrl+Shift+H action.
    #[cfg(feature = "live-view")]
    let quick_entry_menu = MenuItem::new("Log a block…", true, None);
    #[cfg(not(feature = "live-view"))]
    let quick_entry_menu = MenuItem::new("Quick entry  (Ctrl+Shift+H)", true, None);
    #[cfg(feature = "live-view")]
    let live_view_menu = MenuItem::new("Open popover  (Ctrl+Shift+/)", true, None);
    // v0.3: in the shipping build "Quit" no longer kills the app — it just
    // dismisses the popover; the app stays in the tray (hotkeys live) and
    // auto-starts on login (MSIX StartupTask), so Ctrl+Shift+/ always has
    // something to talk to. "Exit Time Tracker" is the real exit. (The
    // no-feature fallback build has no popover, so "Quit" still exits there.)
    #[cfg(feature = "live-view")]
    let quit_menu = MenuItem::new("Hide  (Time Tracker keeps running in the tray)", true, None);
    #[cfg(not(feature = "live-view"))]
    let quit_menu = MenuItem::new("Quit", true, None);
    #[cfg(feature = "live-view")]
    let exit_menu = MenuItem::new("Exit Time Tracker", true, None);
    menu.append(&header).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    #[cfg(feature = "live-view")]
    menu.append(&add_workstream_menu).unwrap();
    menu.append(&timer_start_menu).unwrap();
    menu.append(&timer_stop_menu).unwrap();
    menu.append(&quick_entry_menu).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    #[cfg(feature = "live-view")]
    {
        menu.append(&live_view_menu).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
    }
    menu.append(&quit_menu).unwrap();
    #[cfg(feature = "live-view")]
    menu.append(&exit_menu).unwrap();

    let menu_ids = MenuIds {
        #[cfg(feature = "live-view")]
        add_workstream: add_workstream_menu.id().clone(),
        quick_entry: quick_entry_menu.id().clone(),
        timer_start: timer_start_menu.id().clone(),
        timer_stop: timer_stop_menu.id().clone(),
        #[cfg(feature = "live-view")]
        live_view: live_view_menu.id().clone(),
        quit: quit_menu.id().clone(),
        #[cfg(feature = "live-view")]
        exit: exit_menu.id().clone(),
    };

    let _tray: TrayIcon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(APP_NAME)
        .with_icon(make_tray_icon())
        .build()
        .expect("tray icon failed to build");

    tracing::info!("Tray icon active. Right-click for menu. Hotkeys live.");
    eprintln!("Tray icon active. Right-click for menu. Hotkeys live.");

    // SPEC §3.2: 30s tick persistence. Real `thread::sleep` (parkable
    // by the OS scheduler under EcoQoS — per `pane-power-and-throttling.md`,
    // `yield_now` would prevent that). The proxy posts a TimerTick
    // UserEvent which the main thread handles by calling
    // `popup.tick_persist()` (which delegates to its owned Timer).
    let tick_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("tt-timer-tick".to_string())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if tick_proxy.send_event(UserMessage::TimerTick).is_err() {
                // Event loop closed; we're shutting down.
                break;
            }
        })
        .expect("failed to spawn timer tick thread");

    let popup_window_id = popup.window_id();

    // `usage` is Arc<Usage>; clone for the closure so the outer `usage`
    // remains usable for `usage.app_exit()` after the loop returns.
    let usage_loop = usage.clone();
    let result = event_loop.run(move |event, elwt| {
        // ControlFlow::Wait blocks until the next OS event. We
        // additionally poll hotkey/menu/IPC channels every wake.
        elwt.set_control_flow(ControlFlow::Wait);

        // ---- main event dispatch ----
        match &event {
            Event::WindowEvent { window_id, event: window_event }
                if *window_id == popup_window_id =>
            {
                if let WindowEvent::RedrawRequested = window_event {
                    popup.paint();
                } else {
                    popup.on_window_event(window_event);
                }
            }
            #[cfg(feature = "live-view")]
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested }
                if Some(*window_id) == popover_window_id =>
            {
                // Borderless tool window has no close button, but Alt+F4 while
                // focused still asks — treat it as "dismiss", not "quit".
                if let Some(pw) = popover_window.as_mut() {
                    pw.hide();
                }
            }
            Event::UserEvent(msg) => match msg {
                UserMessage::ShowQuickEntry => popup.show(PopupMode::QuickEntry, "ipc"),
                UserMessage::ShowTimerStart => popup.show(PopupMode::TimerStart, "ipc"),
                UserMessage::ShowTimerStop => popup.show(PopupMode::TimerStop, "ipc"),
                UserMessage::TimerTick => popup.tick_persist(),
                #[cfg(feature = "live-view")]
                UserMessage::TimerCmdStart { client, engagement, billable } => {
                    popup.cmd_timer_start(client, engagement, *billable);
                }
                #[cfg(feature = "live-view")]
                UserMessage::TimerCmdStop => popup.cmd_timer_stop(),
                #[cfg(feature = "live-view")]
                UserMessage::TimerCmdSwitch { client, engagement, billable } => {
                    popup.cmd_timer_switch(client, engagement, *billable);
                }
                #[cfg(feature = "live-view")]
                UserMessage::PopoverHide => {
                    if let Some(pw) = popover_window.as_mut() {
                        pw.hide();
                    }
                }
                #[cfg(feature = "live-view")]
                UserMessage::PopoverOpenRecorded => {
                    open_recorded_in_browser();
                    if let Some(pw) = popover_window.as_mut() {
                        pw.hide();
                    }
                }
                #[cfg(feature = "live-view")]
                UserMessage::PopoverResize(h) => {
                    if let Some(pw) = popover_window.as_mut() {
                        pw.set_content_height(*h);
                    }
                }
            },
            _ => {}
        }

        // ---- channel polling ----
        // Hotkeys. Each fire emits a `hotkey_fire` usage event with
        // success=true (the registration-time failures already emitted
        // success=false). Per SPEC §5.4.
        while let Ok(hk) = GlobalHotKeyEvent::receiver().try_recv() {
            if hk.state != global_hotkey::HotKeyState::Pressed {
                continue;
            }
            if hk.id == hotkey_ids.add_workstream {
                // v0.2/v0.3: Ctrl+Shift+H always lands you in the popover with
                // the add-workstream form expanded. If the native window is
                // already up, keep its state and open the form via the page
                // (Esc returns to that state); if it's hidden, summon it
                // straight into add-mode (Esc then closes the popover).
                // Without the live-view feature there's no popover — fall back
                // to the old quick-entry popup.
                #[cfg(feature = "live-view")]
                {
                    usage_loop.hotkey_fire("add_workstream", true);
                    match popover_window.as_mut() {
                        Some(pw) => {
                            if pw.is_visible() {
                                live_bus.hotkey("add_workstream");
                                pw.show(false);
                            } else {
                                pw.show(true);
                            }
                        }
                        None => {
                            live_bus.hotkey("add_workstream");
                            open_popover_browser_add();
                        }
                    }
                }
                #[cfg(not(feature = "live-view"))]
                {
                    usage_loop.hotkey_fire("quick_entry", true);
                    live_bus.hotkey("quick_entry");
                    popup.show(PopupMode::QuickEntry, "hotkey");
                }
            } else if hk.id == hotkey_ids.timer_start {
                // v0.3: Ctrl+Shift+; no longer pops the start-timer prompt — in
                // practice you start a timer by picking a workstream in the
                // popover (Enter on the list) or via Ctrl+Shift+H. This slot now
                // mirrors Ctrl+Shift+' (stop + write the entry, no popup) so
                // muscle memory for either key just stops the timer.
                usage_loop.hotkey_fire("timer_stop", true);
                live_bus.hotkey("timer_stop");
                #[cfg(feature = "live-view")]
                popup.cmd_timer_stop();
                #[cfg(not(feature = "live-view"))]
                popup.show(PopupMode::TimerStop, "hotkey");
            } else if hk.id == hotkey_ids.timer_stop {
                usage_loop.hotkey_fire("timer_stop", true);
                live_bus.hotkey("timer_stop");
                // v0.3: Ctrl+Shift+' just stops the running timer + writes the
                // entry — no popup. (The no-feature build keeps the old
                // confirm-popup since it has no headless command path.)
                #[cfg(feature = "live-view")]
                popup.cmd_timer_stop();
                #[cfg(not(feature = "live-view"))]
                popup.show(PopupMode::TimerStop, "hotkey");
            } else if hk.id == hotkey_ids.popover_toggle {
                usage_loop.hotkey_fire("popover_toggle", true);
                #[cfg(feature = "live-view")]
                {
                    live_bus.hotkey("popover_toggle");
                    match popover_window.as_mut() {
                        Some(pw) => pw.toggle(),
                        None => open_popover_browser(),
                    }
                }
            }
        }
        // Tray menu
        while let Ok(menu_evt) = MenuEvent::receiver().try_recv() {
            #[cfg(feature = "live-view")]
            if menu_evt.id == menu_ids.live_view {
                match popover_window.as_mut() {
                    Some(pw) => pw.show(false),
                    None => open_popover_browser(),
                }
                continue;
            }
            #[cfg(feature = "live-view")]
            if menu_evt.id == menu_ids.add_workstream {
                match popover_window.as_mut() {
                    Some(pw) => {
                        if pw.is_visible() {
                            live_bus.hotkey("add_workstream");
                            pw.show(false);
                        } else {
                            pw.show(true);
                        }
                    }
                    None => {
                        live_bus.hotkey("add_workstream");
                        open_popover_browser_add();
                    }
                }
                continue;
            }
            #[cfg(feature = "live-view")]
            if menu_evt.id == menu_ids.exit {
                tracing::info!("Exit Time Tracker selected from tray menu. Exiting.");
                eprintln!("[menu] Exit selected. Exiting.");
                elwt.exit();
                continue;
            }
            if menu_evt.id == menu_ids.quit {
                #[cfg(feature = "live-view")]
                {
                    // "Hide" — dismiss the popover, keep the app alive in the tray.
                    if let Some(pw) = popover_window.as_mut() {
                        pw.hide();
                    }
                }
                #[cfg(not(feature = "live-view"))]
                {
                    tracing::info!("Quit selected from tray menu. Exiting.");
                    eprintln!("[menu] Quit selected. Exiting.");
                    elwt.exit();
                }
            } else if menu_evt.id == menu_ids.quick_entry {
                popup.show(PopupMode::QuickEntry, "menu");
            } else if menu_evt.id == menu_ids.timer_start {
                popup.show(PopupMode::TimerStart, "menu");
            } else if menu_evt.id == menu_ids.timer_stop {
                #[cfg(feature = "live-view")]
                popup.cmd_timer_stop();
                #[cfg(not(feature = "live-view"))]
                popup.show(PopupMode::TimerStop, "menu");
            }
        }

        // While the popup is visible, keep the loop awake at modest cadence
        // so suggestion clicks etc. propagate without depending on user input.
        if popup.is_visible() {
            elwt.set_control_flow(ControlFlow::Poll);
        }
    });

    if let Err(e) = result {
        tracing::error!(error = %e, "event loop exited with error");
    }

    usage.app_exit();
    // Hold _mutex_guard until the very end so its Drop runs after the loop.
    drop(_mutex_guard);
}

struct RegisteredHotkeys {
    /// v0.2: Ctrl+Shift+H — opens the popover in add-workstream mode
    /// (was "quick entry" in v0.1; that moved to Recorded Time's
    /// "+ New entry" and the tray "Log a block…" item).
    add_workstream: u32,
    timer_start: u32,
    timer_stop: u32,
    /// v0.2: Ctrl+Shift+/ — opens (toggles) the popover.
    popover_toggle: u32,
}

struct HotkeyRegistration {
    ids: RegisteredHotkeys,
    /// Human labels of hotkeys that failed to register (combo already taken
    /// by another app, etc.). Empty on success. Used to surface conflicts to
    /// the user via MessageBoxW once on first launch.
    failures: Vec<String>,
    /// Stable per-hotkey registration outcomes for the `hotkey_registered`
    /// usage event (R5): `(which, ok)`. `which` matches the labels used by
    /// `hotkey_fire` on a real press, so the two event streams join cleanly.
    outcomes: Vec<(&'static str, bool)>,
}

struct MenuIds {
    #[cfg(feature = "live-view")]
    add_workstream: MenuId,
    quick_entry: MenuId,
    timer_start: MenuId,
    timer_stop: MenuId,
    #[cfg(feature = "live-view")]
    live_view: MenuId,
    quit: MenuId,
    #[cfg(feature = "live-view")]
    exit: MenuId,
}

/// Build a `HotKey` from a `config::HotkeyCombo` (parsed from `[hotkeys]`),
/// or fall back to `default` if the combo string was absent / unparseable /
/// names a `Code` `global-hotkey` doesn't know. Logs the fallback so a
/// typo'd rebind doesn't silently revert with no trace.
fn hotkey_from_config(
    which: &str,
    combo: Option<&str>,
    default: HotKey,
) -> HotKey {
    use std::str::FromStr;
    let Some(s) = combo else { return default };
    let Some(parsed) = config::parse_hotkey_combo(s) else {
        tracing::warn!(which, raw = %s, "config [hotkeys]: unparseable combo — using default");
        return default;
    };
    let Ok(code) = Code::from_str(&parsed.key) else {
        tracing::warn!(which, raw = %s, key = %parsed.key, "config [hotkeys]: unknown key code — using default");
        return default;
    };
    let mut mods = Modifiers::empty();
    if parsed.ctrl { mods |= Modifiers::CONTROL; }
    if parsed.alt { mods |= Modifiers::ALT; }
    if parsed.shift { mods |= Modifiers::SHIFT; }
    if parsed.win { mods |= Modifiers::SUPER; }
    tracing::info!(which, raw = %s, "config [hotkeys]: applied rebind");
    HotKey::new(Some(mods), code)
}

fn register_hotkeys(
    manager: &GlobalHotKeyManager,
    rebinds: &config::Hotkeys,
) -> HotkeyRegistration {
    // v0.2 hardcoded defaults — overridden per-action by `[hotkeys]` in
    // config.toml when present (R2). `Code::Slash` is the physical `/` key;
    // on a US QWERTY layout Ctrl+Shift+/ and Ctrl+Shift+? are the same
    // physical chord, so this won't double-fire. (International layouts: a
    // v1.0 concern — `global-hotkey` keys by physical code, not the produced
    // character.)
    let add_workstream = hotkey_from_config(
        "quick_entry",
        rebinds.quick_entry.as_deref(),
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyH),
    );
    let timer_start = hotkey_from_config(
        "timer_start",
        rebinds.timer_start.as_deref(),
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Semicolon),
    );
    let timer_stop = hotkey_from_config(
        "timer_stop",
        rebinds.timer_stop.as_deref(),
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Quote),
    );
    let popover_toggle = hotkey_from_config(
        "popover_toggle",
        rebinds.popover_toggle.as_deref(),
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Slash),
    );

    let mut failures = Vec::new();
    let mut outcomes = Vec::new();
    register_or_warn(manager, add_workstream, "add_workstream", "Ctrl+Shift+H (add workstream)", &mut failures, &mut outcomes);
    register_or_warn(manager, timer_start, "timer_start", "Ctrl+Shift+; (timer start)", &mut failures, &mut outcomes);
    register_or_warn(manager, timer_stop,  "timer_stop", "Ctrl+Shift+' (timer stop)",  &mut failures, &mut outcomes);
    register_or_warn(manager, popover_toggle, "popover_toggle", "Ctrl+Shift+/ (popover)",   &mut failures, &mut outcomes);

    HotkeyRegistration {
        ids: RegisteredHotkeys {
            add_workstream: add_workstream.id(),
            timer_start: timer_start.id(),
            timer_stop: timer_stop.id(),
            popover_toggle: popover_toggle.id(),
        },
        failures,
        outcomes,
    }
}

fn register_or_warn(
    manager: &GlobalHotKeyManager,
    hk: HotKey,
    which: &'static str,
    label: &str,
    failures: &mut Vec<String>,
    outcomes: &mut Vec<(&'static str, bool)>,
) {
    if let Err(e) = manager.register(hk) {
        tracing::warn!(label, error = %e, "hotkey registration failed");
        eprintln!("WARNING: failed to register {label}: {e}");
        failures.push(label.to_string());
        outcomes.push((which, false));
    } else {
        tracing::info!(label, "hotkey registered");
        outcomes.push((which, true));
    }
}

/// R2: First-launch MessageBoxW shown only when one or more hotkeys
/// failed to register. Zero-noise on the success path. Body points the
/// user at the config file for rebinding (Settings UI is v1.1).
fn show_hotkey_conflict_dialog(failures: &[String]) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONWARNING, MB_OK,
    };

    let title = "Time Tracker — hotkey conflict";
    let body = format!(
        "Some hotkeys could not register on this machine, usually because another app already \
         claimed the combo (Excel, Logitech Options, AHK, IME):\n\n\
         {failed}\n\n\
         The tracker will still run — affected hotkeys won't fire. To rebind, edit:\n\n\
         %LOCALAPPDATA%\\TimeTracker\\config.toml\n\n\
         (Or use the tray menu / right-click options to access the same actions.)",
        failed = failures
            .iter()
            .map(|f| format!("    • {f}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let title_w: Vec<u16> = std::ffi::OsStr::new(title)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let body_w: Vec<u16> = std::ffi::OsStr::new(&body)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | MB_ICONWARNING,
        );
    }
}

/// Tray icon = the "db" brand mark (same glyph as the in-app favicon).
///
/// We bake a 32x32 RGBA bitmap at build time from `src/favicon.svg` (rendered
/// via `rsvg-convert`, then dumped to raw RGBA — see
/// `scripts/regen-tray-icon.sh`). Embedding the raw buffer avoids pulling a
/// PNG decoder or a font renderer into the runtime: it's exactly 4096 bytes
/// (32*32*4) sitting in .rodata, and `tray-icon` is happy to hand them
/// straight to Windows. If the SVG changes, regenerate the .rgba.
fn make_tray_icon() -> Icon {
    const SIZE: u32 = 32;
    const RGBA: &[u8] = include_bytes!("tray-icon-32.rgba");
    // Defensive: a wrong-sized file would silently produce a corrupt icon
    // otherwise; this assertion only ever fires if someone regenerates the
    // .rgba with a non-32 size.
    debug_assert_eq!(RGBA.len(), (SIZE * SIZE * 4) as usize);
    Icon::from_rgba(RGBA.to_vec(), SIZE, SIZE).expect("tray icon")
}

fn install_ctrlc_handler() {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };

    unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
        if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
            eprintln!();
            eprintln!("[Ctrl+C - exiting cleanly]");
            std::process::exit(0);
        }
        0
    }
    let _ = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}
