// SPEC §7 items 6, 7, 8, 11 - quick-entry popup window.
//
// Built on primitives (per project memory: build-on-primitives):
//   winit::Window           - the popup window (one of two windows the
//                             event loop manages; tray icon + tray menu
//                             are separate Win32 windows hidden under
//                             tray-icon abstraction)
//   glutin                  - OpenGL context creation against the winit
//                             Window, manually wired through
//                             raw-window-handle 0.6
//   glow                    - GL bindings (egui_glow::Painter renders
//                             onto this)
//   egui_glow::Painter      - draws egui frames into the GL surface
//   egui_winit::State       - translates winit::WindowEvent into egui's
//                             input model
//   egui::Context           - immediate-mode UI (form fields, buttons,
//                             keyboard handling)
//
// Pre-warm strategy (per SPEC §4.2 <100ms budget):
//   The window is created hidden at app start. show(mode) flips it
//   visible + focused. hide() flips it back. egui_glow's first paint
//   compiles shaders and builds the font atlas; that work is amortized
//   over the always-running tray lifetime, so the FIRST hotkey fire
//   doesn't pay the startup cost.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Local;
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, Version,
};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasRawWindowHandle;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::EventLoopWindowTarget;
use winit::window::{Window, WindowBuilder, WindowId};

use time_tracker::{autocomplete, csv_writer, duration, timer, usage, workstream};

use super::LiveBus;

const POPUP_W: f32 = 460.0;
const POPUP_H: f32 = 280.0;
const TOAST_DURATION: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupMode {
    QuickEntry,
    TimerStart,
    TimerStop,
}

impl PopupMode {
    fn title(self) -> &'static str {
        match self {
            PopupMode::QuickEntry => "Quick entry",
            PopupMode::TimerStart => "Start timer",
            PopupMode::TimerStop => "Stop timer",
        }
    }
    fn shows_duration_field(self) -> bool {
        // SPEC §3.2: timer-start popup omits duration; timer-stop and
        // quick-entry both show it.
        !matches!(self, PopupMode::TimerStart)
    }
}

#[derive(Debug, Default)]
struct Fields {
    client: String,
    engagement: String,
    narrative: String,
    duration_input: String,
    billable: bool,
    error: Option<String>,
}

impl Fields {
    fn reset(&mut self, billable_default: bool) {
        self.client.clear();
        self.engagement.clear();
        self.narrative.clear();
        self.duration_input.clear();
        self.billable = billable_default;
        self.error = None;
    }
}

pub struct Popup {
    window: Window,
    gl_surface: Surface<WindowSurface>,
    gl_context: PossiblyCurrentContext,
    egui_glow: egui_glow::EguiGlow,

    csv_writer: csv_writer::WriterHandle,
    autocomplete: autocomplete::Cache,
    timer: timer::Timer,
    staff_name: String,
    usage: Arc<usage::Usage>,
    live_bus: LiveBus,
    /// v0.2 workstream registry (shared with the localhost server).
    /// Touched/extended whenever a timer starts or an entry is written so
    /// `last_used_at` stays fresh and new (client, engagement) pairs
    /// register automatically.
    workstreams: Arc<Mutex<workstream::WorkstreamRegistry>>,

    mode: PopupMode,
    visible: bool,
    fields: Fields,
    last_toast: Option<String>,
    toast_until: Option<Instant>,
    open_at: Option<Instant>,
}

impl Popup {
    pub fn new<T>(
        event_loop: &EventLoopWindowTarget<T>,
        csv_writer: csv_writer::WriterHandle,
        autocomplete: autocomplete::Cache,
        timer: timer::Timer,
        staff_name: String,
        usage: Arc<usage::Usage>,
        live_bus: LiveBus,
        workstreams: Arc<Mutex<workstream::WorkstreamRegistry>>,
    ) -> Self {
        // ----- create winit::Window via DisplayBuilder so glutin gets the
        //       config it needs in the same step -----
        let window_builder = WindowBuilder::new()
            .with_title("Time Tracker")
            .with_inner_size(LogicalSize::new(POPUP_W, POPUP_H))
            .with_min_inner_size(LogicalSize::new(POPUP_W, POPUP_H))
            .with_resizable(false)
            .with_visible(false); // pre-warmed

        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_transparency(false);

        let display_builder = DisplayBuilder::new().with_window_builder(Some(window_builder));

        let (maybe_window, gl_config) = display_builder
            .build(event_loop, template, |configs| {
                configs
                    .reduce(|best, c| if c.num_samples() > best.num_samples() { c } else { best })
                    .expect("at least one GL config")
            })
            .expect("DisplayBuilder failed");

        let window = maybe_window.expect("window not created by DisplayBuilder");

        let gl_display = gl_config.display();

        // ----- create the OpenGL context -----
        // OpenGL 3.3 core is a safe target on every Windows GPU since ~2010.
        // WGL on Windows requires the context creation to know the window
        // handle (HDC pixel-format scoping); passing None fails with
        // "invalid pixel format" (os err 2000). winit::Window's
        // raw_window_handle() method returns the rwh-0.5 type that glutin
        // 0.31 expects (egui-winit pulls rwh-0.6 separately - independent
        // dep graphs, no cross-boundary type mixing).
        let raw_handle = window.raw_window_handle();
        let context_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(Some(raw_handle));
        let not_current_context = unsafe {
            gl_display
                .create_context(&gl_config, &context_attrs)
                .expect("create_context failed")
        };

        // ----- create the GL window surface -----
        let surface_attrs =
            window.build_surface_attributes(SurfaceAttributesBuilder::default());
        let gl_surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .expect("create_window_surface failed")
        };

        let gl_context = not_current_context
            .make_current(&gl_surface)
            .expect("make_current failed");

        // ----- glow context for egui_glow -----
        let glow_ctx = Arc::new(unsafe {
            glow::Context::from_loader_function_cstr(|s| gl_display.get_proc_address(s))
        });

        // ----- egui_glow (Painter + egui-winit::State, all in one) -----
        let egui_glow = egui_glow::EguiGlow::new(event_loop, glow_ctx, None, None);

        let mut fields = Fields::default();
        fields.billable = true; // SPEC §3.1 default

        Self {
            window,
            gl_surface,
            gl_context,
            egui_glow,
            csv_writer,
            autocomplete,
            timer,
            staff_name,
            usage,
            live_bus,
            workstreams,
            mode: PopupMode::QuickEntry,
            visible: false,
            fields,
            last_toast: None,
            toast_until: None,
            open_at: None,
        }
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// SPEC §3.2: re-persist running timer state on a 30s cadence so
    /// crashes never lose elapsed time beyond the 30s window. Cheap
    /// no-op when no timer is running. Called from the main event loop
    /// in response to a `UserMessage::TimerTick` posted by a dedicated
    /// 30s sleep thread.
    pub fn tick_persist(&self) {
        self.timer.tick_persist();
    }

    // ---- v0.3: headless timer commands driven by the popover -------------
    // These mutate the same Timer the hotkeys/popup use and emit the same
    // broadcast events, but never show the popup window. Invoked from the
    // main event loop in response to `UserMessage::TimerCmd*` (posted by the
    // /timer/* HTTP handlers after they've validated the workstream id).

    /// POST /timer/start: start a timer on (client, engagement). If one is
    /// already running, this becomes a switch instead (the popover should
    /// have used /timer/switch, but be forgiving). No narrative yet — the
    /// user fills one in on stop, or edits the row in Recorded Time.
    #[cfg(feature = "live-view")]
    pub fn cmd_timer_start(&mut self, client: &str, engagement: &str, billable: bool) {
        if self.timer.is_running() {
            self.cmd_timer_switch(client, engagement, billable);
            return;
        }
        let (client, engagement) = (client.trim(), engagement.trim());
        if client.is_empty() {
            tracing::warn!("cmd_timer_start with empty client — ignored");
            return;
        }
        let narrative = String::new();
        if let Err(e) = self
            .timer
            .start(client.to_string(), engagement.to_string(), narrative.clone(), billable)
        {
            tracing::warn!(error = e, "cmd_timer_start: timer.start failed");
            return;
        }
        let _ = self.autocomplete.observe(client, engagement);
        self.touch_workstream(client, engagement, billable);
        self.live_bus.timer_started(client, engagement, &narrative);
        tracing::info!(client, engagement, "timer started via popover");
    }

    /// POST /timer/stop: stop the running timer and write the entry (raw
    /// elapsed minutes, ≥1). No-op if nothing is running. On a CSV write
    /// error the timer is left running so elapsed time isn't lost.
    #[cfg(feature = "live-view")]
    pub fn cmd_timer_stop(&mut self) {
        self.stop_running_and_log("popover");
    }

    /// Stop the running timer + write its Entry, exactly like the popover's
    /// stop. Not feature-gated so the continuation-reminder auto-stop works
    /// in the tray-only build too (the `live_bus.*` calls are no-ops without
    /// the `live-view` feature). `reason` is for the log line only — the
    /// elapsed minutes are the raw value (capped at the 12h sanity bound),
    /// so the user's "log capped at 5h" choice falls out naturally: at the
    /// 5h auto-stop the entry is ~300 minutes. No-op if idle; on a CSV
    /// error the timer is left running so elapsed time isn't lost.
    pub fn stop_running_and_log(&mut self, reason: &'static str) {
        let Some(running) = self.timer.peek().cloned() else {
            tracing::info!(reason, "stop_running_and_log: no timer running — no-op");
            return;
        };
        let minutes = running.elapsed_minutes().0.max(1);
        let workstream_id =
            Some(self.touch_workstream(&running.client, &running.engagement, running.billable));
        let entry = csv_writer::Entry {
            timestamp: Local::now(),
            staff: self.staff_name.clone(),
            client: running.client.clone(),
            engagement: running.engagement.clone(),
            narrative: running.narrative.clone(),
            minutes,
            billable: running.billable,
            entry_method: csv_writer::EntryMethod::Timer,
            workstream_id,
        };
        let timestamp_iso = entry.timestamp.to_rfc3339();
        match self.csv_writer.write_blocking(entry) {
            csv_writer::WriteResult::Written => {
                let _ = self.timer.stop();
                self.live_bus.timer_stopped(&running.client, i64::from(minutes));
                self.live_bus.entry_logged(
                    &timestamp_iso,
                    &self.staff_name,
                    &running.client,
                    &running.engagement,
                    &running.narrative,
                    i64::from(minutes),
                    running.billable,
                    "timer",
                );
                self.usage.entry_written("timer", minutes, running.billable);
                tracing::info!(client = %running.client, minutes, reason, "timer stopped");
            }
            csv_writer::WriteResult::Queued => {
                let _ = self.timer.stop();
                self.live_bus.timer_stopped(&running.client, i64::from(minutes));
                self.usage.entry_queued("csv_locked");
                tracing::info!(client = %running.client, minutes, reason, "timer stopped (entry queued)");
            }
            csv_writer::WriteResult::Error(msg) => {
                tracing::error!(error = %msg, reason, "stop_running_and_log: CSV write failed — timer left running");
            }
        }
    }

    // ---- v0.3.1: continuation-reminder bridge ---------------------------
    // Thin delegators to the owned Timer so windows_main's 30s tick can
    // drive the 4h prompt / 5h auto-stop without reaching into `timer`.

    /// Continuation state of the running timer (None when idle).
    pub fn continuation_state(&self) -> Option<timer::ContinuationState> {
        self.timer.continuation_state()
    }

    /// (client, engagement) of the running timer, for the toast text.
    pub fn running_summary(&self) -> Option<(String, String)> {
        self.timer
            .peek()
            .map(|r| (r.client.clone(), r.engagement.clone()))
    }

    /// The continuation toast was shown — don't re-toast during the grace.
    pub fn mark_reminded(&mut self) {
        self.timer.mark_reminded();
    }

    /// User chose "Keep going" — re-arm the window for another 4h.
    pub fn ack_continue(&mut self) {
        self.timer.ack_continue();
        tracing::info!("continuation: user confirmed — window re-armed");
    }

    /// POST /timer/switch: stop the current timer (writing its entry), then
    /// start a new one on (client, engagement) with a "Switched from {prev}"
    /// narrative. Emits the usual stop/log/start trio plus `workstream_switched`.
    /// If the stop failed (CSV error), the old timer is kept and no switch
    /// happens — better than losing the elapsed time or double-starting.
    #[cfg(feature = "live-view")]
    pub fn cmd_timer_switch(&mut self, client: &str, engagement: &str, billable: bool) {
        let prev_engagement = self.timer.peek().map(|r| r.engagement.clone());
        if self.timer.is_running() {
            self.cmd_timer_stop();
            if self.timer.is_running() {
                tracing::warn!("cmd_timer_switch: stop failed — switch aborted");
                return;
            }
        }
        let (client, engagement) = (client.trim(), engagement.trim());
        if client.is_empty() {
            tracing::warn!("cmd_timer_switch with empty client — ignored");
            return;
        }
        // No "Switched from {prev}" narrative — the popover doesn't show it and
        // it just clutters the timesheet; the switch is conveyed by the
        // workstream_switched event + the popover's 1s "switched" toast.
        let narrative = String::new();
        if let Err(e) = self
            .timer
            .start(client.to_string(), engagement.to_string(), narrative.clone(), billable)
        {
            tracing::warn!(error = e, "cmd_timer_switch: timer.start failed");
            return;
        }
        let _ = self.autocomplete.observe(client, engagement);
        self.touch_workstream(client, engagement, billable);
        self.live_bus.timer_started(client, engagement, &narrative);
        self.live_bus
            .workstream_switched(prev_engagement.as_deref().unwrap_or(""), client, engagement);
        tracing::info!(client, engagement, "timer switched via popover");
    }

    /// `source` is the SPEC §5.4 `popup_open.source` enum: `"hotkey"`,
    /// `"menu"`, or `"ipc"` — *where the show came from*, not which popup it
    /// is. (Previously this logged the popup *mode* by mistake — R5.)
    pub fn show(&mut self, mode: PopupMode, source: &'static str) {
        // SPEC §3.2: handle wrong-state cases. Show the popup with just
        // the toast; user dismisses with Esc. Reset fields so stale
        // form state isn't visible alongside the warning.
        match mode {
            PopupMode::TimerStart if self.timer.is_running() => {
                let client = self
                    .timer
                    .peek()
                    .map(|r| r.client.clone())
                    .unwrap_or_default();
                tracing::info!("timer-start pressed while timer is running - no-op");
                self.fields.reset(true);
                self.set_toast(format!(
                    "Timer already running for {client}. Press Ctrl+Shift+' to stop."
                ));
                self.visible = true;
                self.window.set_visible(true);
                self.window.focus_window();
                self.window.request_redraw();
                self.mode = PopupMode::TimerStart;
                return;
            }
            PopupMode::TimerStop if !self.timer.is_running() => {
                tracing::info!("timer-stop pressed but no timer running - no-op");
                self.fields.reset(true);
                self.set_toast("No timer running. Press Ctrl+Shift+; to start.".into());
                self.visible = true;
                self.window.set_visible(true);
                self.window.focus_window();
                self.window.request_redraw();
                self.mode = PopupMode::TimerStop;
                return;
            }
            _ => {}
        }

        self.mode = mode;
        self.fields.reset(true);

        // SPEC §3.2: TimerStop pre-fills fields + duration from the
        // running timer. The duration is rounded to the nearest 0.1h.
        if matches!(mode, PopupMode::TimerStop) {
            if let Some(running) = self.timer.peek() {
                self.fields.client = running.client.clone();
                self.fields.engagement = running.engagement.clone();
                self.fields.narrative = running.narrative.clone();
                self.fields.billable = running.billable;
                let (mins, exceeded_cap) = running.elapsed_minutes();
                let hours = (mins as f64) / 60.0;
                let rounded = (hours * 10.0).round() / 10.0;
                self.fields.duration_input = format!("{rounded:.1}");
                if exceeded_cap {
                    self.fields.error =
                        Some("Timer running >12h - confirm or edit duration.".into());
                }
            }
        }

        self.visible = true;
        self.open_at = Some(Instant::now());
        self.position_on_cursor_monitor();
        self.window.set_visible(true);
        self.window.focus_window();
        self.window.request_redraw();
        self.usage.popup_open(source);
        tracing::info!(target: "ui", ?mode, %source, "popup shown");
    }

    /// SPEC §3.1/§3.2/§4.3 + the "popup renders on the cursor's monitor"
    /// invariant: place the popup centered on whichever monitor currently
    /// holds the cursor (`MonitorFromPoint(GetCursorPos, MONITOR_DEFAULTTONEAREST)`),
    /// not winit's last position (which is the OS default — typically the
    /// primary monitor's center). Best-effort: a Win32/winit miss leaves the
    /// window where it was, which is no worse than before.
    fn position_on_cursor_monitor(&self) {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::POINT;
            use windows_sys::Win32::Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
            };
            use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
            unsafe {
                let mut pt = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut pt) == 0 {
                    return;
                }
                let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                if hmon.is_null() {
                    return;
                }
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                if GetMonitorInfoW(hmon, &mut mi) == 0 {
                    return;
                }
                // Work area in physical pixels; center the popup's physical size in it.
                let scale = self.window.scale_factor();
                let win_w = (POPUP_W as f64 * scale).round() as i32;
                let win_h = (POPUP_H as f64 * scale).round() as i32;
                let wa = mi.rcWork;
                let wa_w = (wa.right - wa.left).max(win_w);
                let wa_h = (wa.bottom - wa.top).max(win_h);
                let x = wa.left + (wa_w - win_w) / 2;
                let y = wa.top + (wa_h - win_h) / 2;
                self.window
                    .set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }
    }

    pub fn close(&mut self, reason: &str) {
        if self.visible {
            let dur_ms = self.open_at.map(|t| t.elapsed().as_millis()).unwrap_or(0);
            self.usage.popup_close(reason, dur_ms);
        }
        self.visible = false;
        self.open_at = None;
        self.window.set_visible(false);
    }

    /// Forward a winit window event to egui-winit. Returns true if the
    /// event was consumed (caller need not propagate further).
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        // GL surface needs resizing when the window resizes (even though
        // the popup is non-resizable, DPI changes can trigger this).
        if let WindowEvent::Resized(size) = event {
            if size.width > 0 && size.height > 0 {
                self.gl_surface.resize(
                    &self.gl_context,
                    NonZeroU32::new(size.width).unwrap(),
                    NonZeroU32::new(size.height).unwrap(),
                );
            }
        }
        if matches!(event, WindowEvent::CloseRequested) {
            // For the popup, the X button means cancel, not exit.
            self.close("cancel");
            return;
        }
        let _ = self.egui_glow.on_window_event(&self.window, event);
    }

    /// Run one paint cycle: build the egui frame, paint to the GL
    /// surface, swap buffers. No-op when the window is hidden (saves
    /// CPU + skips the swap which would otherwise no-op anyway).
    pub fn paint(&mut self) {
        if !self.visible {
            return;
        }
        // Toast lifecycle
        if let Some(until) = self.toast_until {
            if std::time::Instant::now() >= until {
                self.last_toast = None;
                self.toast_until = None;
            }
        }

        // Capture closures need mutable access to self via partial borrows
        // (egui_glow.run wants &Window + a closure that owns its captures).
        // Split borrows manually so the closure can mutate fields/etc.
        let fields = &mut self.fields;
        let mode = self.mode;
        let last_toast = self.last_toast.clone();
        let autocomplete = &self.autocomplete;
        let mut submit_clicked = false;
        let mut cancel_clicked = false;
        let mut suggestion_chosen_client: Option<String> = None;
        let mut suggestion_chosen_engagement: Option<String> = None;

        self.egui_glow.run(&self.window, |ctx| {
            // Keyboard shortcuts at the top - so they fire even when no
            // text input has focus.
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel_clicked = true;
            }
            if ctx.input(|i| {
                i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.ctrl
            }) {
                submit_clicked = true;
            }

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading(mode.title());
                ui.add_space(6.0);

                // Field order = tab order. Client -> Narrative -> Duration are
                // the everyday path (narrative is required; engagement is not),
                // so Tab out of Client lands on Narrative. Engagement is
                // deprioritized below them; Billable last (it has a default).
                ui.horizontal(|ui| {
                    ui.label("Client:");
                    ui.add(
                        egui::TextEdit::singleline(&mut fields.client)
                            .desired_width(f32::INFINITY)
                            .hint_text("ClientCo"),
                    );
                });
                draw_suggestions(
                    ui,
                    &fields.client,
                    &autocomplete.rank_clients(&fields.client),
                    &mut suggestion_chosen_client,
                );

                ui.label("Narrative:");
                ui.add(
                    egui::TextEdit::multiline(&mut fields.narrative)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                );

                if mode.shows_duration_field() {
                    ui.horizontal(|ui| {
                        ui.label("Duration:");
                        ui.add(
                            egui::TextEdit::singleline(&mut fields.duration_input)
                                .desired_width(140.0)
                                .hint_text("0.3 / 18m / 1h30m / 1:15"),
                        );
                    });
                }

                ui.horizontal(|ui| {
                    ui.label("Engagement:");
                    ui.add(
                        egui::TextEdit::singleline(&mut fields.engagement)
                            .desired_width(f32::INFINITY)
                            .hint_text("(optional)"),
                    );
                });
                draw_suggestions(
                    ui,
                    &fields.engagement,
                    &autocomplete.rank_engagements(&fields.engagement),
                    &mut suggestion_chosen_engagement,
                );

                ui.checkbox(&mut fields.billable, "Billable");

                if let Some(err) = &fields.error {
                    ui.colored_label(egui::Color32::RED, err);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Submit (Enter)").clicked() {
                        submit_clicked = true;
                    }
                    if ui.button("Cancel (Esc)").clicked() {
                        cancel_clicked = true;
                    }
                });
            });

            // Toast on top of everything (clone-by-ref since closure is FnMut)
            if let Some(msg) = last_toast.as_ref() {
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -16.0])
                    .show(ctx, |ui| {
                        egui::Frame::popup(&ctx.style())
                            .fill(egui::Color32::from_black_alpha(220))
                            .show(ui, |ui| {
                                ui.colored_label(egui::Color32::WHITE, msg);
                            });
                    });
            }
        });

        // Apply suggestion clicks (we couldn't mutate &mut fields.X
        // inside the closure that already had &mut fields)
        if let Some(s) = suggestion_chosen_client {
            self.fields.client = s;
        }
        if let Some(s) = suggestion_chosen_engagement {
            self.fields.engagement = s;
        }

        // Submit / cancel actions
        if cancel_clicked {
            self.close("cancel");
        } else if submit_clicked {
            self.try_submit();
        }

        // Paint to the GL surface and swap.
        self.egui_glow.paint(&self.window);
        let _ = self.gl_surface.swap_buffers(&self.gl_context);

        // Keep the window damaged while it's visible so input animations
        // (cursor blink, button hover) refresh smoothly.
        if self.visible {
            self.window.request_redraw();
        }
    }

    fn try_submit(&mut self) {
        let client = self.fields.client.trim().to_string();
        if client.is_empty() {
            self.fields.error = Some("Client is required".into());
            return;
        }
        let narrative = self.fields.narrative.trim().to_string();
        if narrative.is_empty() {
            self.fields.error = Some("Narrative is required".into());
            return;
        }
        let engagement = self.fields.engagement.trim().to_string();

        match self.mode {
            PopupMode::TimerStart => {
                // SPEC §3.2: start the timer in memory + persist. No
                // CSV write yet - that happens on stop.
                if let Err(e) = self.timer.start(
                    client.clone(),
                    engagement.clone(),
                    narrative.clone(),
                    self.fields.billable,
                ) {
                    self.fields.error = Some(format!("Timer: {e}"));
                    return;
                }
                let _ = self.autocomplete.observe(&client, &engagement);
                self.touch_workstream(&client, &engagement, self.fields.billable);
                self.live_bus.timer_started(&client, &engagement, &narrative);
                self.set_toast(format!("Timer started for {client}"));
                self.close("submit");
            }
            PopupMode::QuickEntry | PopupMode::TimerStop => {
                let minutes = match duration::parse(&self.fields.duration_input) {
                    Ok(m) => m,
                    Err(e) => {
                        self.fields.error = Some(format!("Duration: {e}"));
                        return;
                    }
                };

                let entry_method = match self.mode {
                    PopupMode::QuickEntry => csv_writer::EntryMethod::Quick,
                    PopupMode::TimerStop => csv_writer::EntryMethod::Timer,
                    PopupMode::TimerStart => unreachable!(),
                };

                // v0.2: resolve (or create) the workstream and stamp its
                // id onto the row. Also bumps `last_used_at`.
                let workstream_id =
                    Some(self.touch_workstream(&client, &engagement, self.fields.billable));

                let entry = csv_writer::Entry {
                    timestamp: Local::now(),
                    staff: self.staff_name.clone(),
                    client: client.clone(),
                    engagement: engagement.clone(),
                    narrative: narrative.clone(),
                    minutes,
                    billable: self.fields.billable,
                    entry_method,
                    workstream_id,
                };
                let timestamp_iso = entry.timestamp.to_rfc3339();
                let billable = self.fields.billable;

                let method_str = match entry_method {
                    csv_writer::EntryMethod::Quick => "quick",
                    csv_writer::EntryMethod::Timer => "timer",
                };

                match self.csv_writer.write_blocking(entry) {
                    csv_writer::WriteResult::Written => {
                        let _ = self.autocomplete.observe(&client, &engagement);
                        if matches!(self.mode, PopupMode::TimerStop) {
                            let _ = self.timer.stop();
                            self.live_bus.timer_stopped(&client, i64::from(minutes));
                        }
                        // entry_logged only after the row is actually on disk
                        // (the dashboard's source of truth is the CSV).
                        self.live_bus.entry_logged(
                            &timestamp_iso,
                            &self.staff_name,
                            &client,
                            &engagement,
                            &narrative,
                            i64::from(minutes),
                            billable,
                            method_str,
                        );
                        self.usage.entry_written(method_str, minutes, self.fields.billable);
                        let hours = (minutes as f64) / 60.0;
                        self.set_toast(format!("Logged {hours:.1}h to {client}"));
                        self.close("submit");
                    }
                    csv_writer::WriteResult::Queued => {
                        let _ = self.autocomplete.observe(&client, &engagement);
                        if matches!(self.mode, PopupMode::TimerStop) {
                            let _ = self.timer.stop();
                            // Timer is stopped even though the row is still
                            // queued; reflect that on the dashboard. No
                            // entry_logged until the queue drains to disk.
                            self.live_bus.timer_stopped(&client, i64::from(minutes));
                        }
                        self.usage.entry_queued("csv_locked");
                        self.set_toast("CSV locked - queued, will retry".into());
                        self.close("submit");
                    }
                    csv_writer::WriteResult::Error(msg) => {
                        tracing::error!(error = %msg, "CSV write failed");
                        self.fields.error = Some(format!("Write failed: {msg}"));
                    }
                }
            }
        }
    }

    fn set_toast(&mut self, msg: String) {
        self.last_toast = Some(msg);
        self.toast_until = Some(std::time::Instant::now() + TOAST_DURATION);
    }

    /// v0.2: register (or refresh) the workstream for this (client,
    /// engagement) pair and return its id. Poison-tolerant; a failed
    /// persist is logged, not fatal — the registry rebuilds from the
    /// CSVs on next launch anyway.
    fn touch_workstream(&self, client: &str, engagement: &str, billable: bool) -> String {
        let mut reg = self.workstreams.lock().unwrap_or_else(|p| p.into_inner());
        let (id, _created) = reg.touch_or_create(client, engagement, billable);
        if let Err(e) = reg.persist() {
            tracing::warn!(error = %e, "workstreams.json persist failed");
        }
        id
    }
}

fn draw_suggestions(
    ui: &mut egui::Ui,
    query: &str,
    suggestions: &[String],
    chosen: &mut Option<String>,
) {
    let q = query.trim();
    if q.is_empty() {
        return;
    }
    if suggestions.is_empty() {
        return;
    }
    if suggestions.len() == 1 && suggestions[0].eq_ignore_ascii_case(q) {
        return;
    }
    ui.indent("autocomplete-rows", |ui| {
        ui.horizontal_wrapped(|ui| {
            for s in suggestions {
                if ui.small_button(s).clicked() {
                    *chosen = Some(s.clone());
                }
            }
        });
    });
}

// staff name now comes from config::Config::identity::staff (read once
// at startup in main.rs, passed into Popup::new).
