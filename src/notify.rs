// v0.3.1: the 4-hour continuation reminder.
//
// A non-invasive Windows toast (Windows.UI.Notifications) asking the user
// to confirm the running timer is still legit. Two buttons -> "Keep going"
// (re-arm the 4h window) and "Stop & log" (stop now). If the user never
// answers, windows_main's 30s tick auto-stops at the 5h mark regardless of
// whether this toast ever displayed -- the toast is best-effort UI; the
// cap is enforced logic.
//
// In-process activation: we keep the ToastNotification object alive in a
// static and register its `Activated` event directly, so no COM activator
// CLSID / out-of-proc handler is needed. This works while the app is
// running (it always is -- it owns the timer). Under MSIX the package
// identity supplies the AUMID automatically; unpackaged dev runs fall back
// to an explicit AUMID (toast may not render without a Start-menu shortcut,
// which is fine -- the 5h auto-stop still fires).
//
// Windows-only. main.rs is #[cfg(windows)]-gated; this module is too.

use std::sync::Mutex;

use windows::core::{Interface, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
};

/// Fallback AUMID for unpackaged runs. Mirrors the MSIX manifest's
/// Identity Name + Application Id so packaged and unpackaged agree.
const AUMID: &str = "RyanStewart.TimeTracker";

/// Which button the user pressed. A plain dismiss (X / timeout / swipe)
/// produces no `ToastAction` -- intentionally, so "no response" stays "no
/// response" and the 5h auto-stop is what handles silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastAction {
    KeepGoing,
    Stop,
}

// Keeping the shown ToastNotification alive is what keeps the in-process
// `Activated` handler registered. Dropping it would silently detach the
// callback, so the previous toast lives here until the next one replaces
// it (only ever one continuation reminder outstanding at a time).
static LAST_TOAST: Mutex<Option<ToastNotification>> = Mutex::new(None);

/// Best-effort: tag this process with an explicit AppUserModelID so
/// unpackaged builds can raise toasts. No-op effect under MSIX (the
/// package identity already supplies one). Safe to call repeatedly.
pub fn set_app_user_model_id() {
    // SetCurrentProcessExplicitAppUserModelID lives in windows-sys, which
    // is already a dependency; avoid pulling the same API in twice.
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    let wide: Vec<u16> = AUMID.encode_utf16().chain(std::iter::once(0)).collect();
    // Returns an HRESULT; a failure here just means unpackaged toasts may
    // not display -- not fatal, the auto-stop safety net is independent.
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(wide.as_ptr()) };
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Show the continuation reminder. `on_action` is invoked from the toast's
/// `Activated` callback with the button the user pressed; it must be cheap
/// and thread-safe (the real handler just forwards to the event loop via
/// an EventLoopProxy). Errors are logged, never propagated -- a failed
/// toast must not break the tick.
pub fn show_continuation_reminder<F>(client: &str, engagement: &str, on_action: F)
where
    F: Fn(ToastAction) + Send + Sync + 'static,
{
    if let Err(e) = try_show(client, engagement, on_action) {
        tracing::warn!(error = %e, "continuation toast failed to show (auto-stop still enforced)");
    }
}

fn try_show<F>(client: &str, engagement: &str, on_action: F) -> windows::core::Result<()>
where
    F: Fn(ToastAction) + Send + Sync + 'static,
{
    let what = if engagement.is_empty() {
        client.to_string()
    } else {
        format!("{client} \u{00b7} {engagement}")
    };
    let body = format!(
        "Still working on \u{201c}{}\u{201d}? It's been 4 hours. If you don't answer, the timer stops and logs in 1 hour.",
        xml_escape(&what)
    );
    let toast_xml = format!(
        r#"<toast activationType="foreground" launch="dismiss" scenario="reminder">
  <visual>
    <binding template="ToastGeneric">
      <text>Timer still running</text>
      <text>{body}</text>
    </binding>
  </visual>
  <actions>
    <action content="Keep going" arguments="keep" activationType="foreground"/>
    <action content="Stop &amp; log" arguments="stop" activationType="foreground"/>
  </actions>
</toast>"#
    );

    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(&toast_xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;

    let handler = TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(
        move |_sender, args| {
            // `args` is the ToastActivatedEventArgs; its `Arguments` is the
            // `arguments=` of the clicked button (or our `launch=` on a
            // body click / dismiss, which we ignore).
            if let Some(inspectable) = args.as_ref() {
                if let Ok(activated) = inspectable.cast::<ToastActivatedEventArgs>() {
                    if let Ok(arg) = activated.Arguments() {
                        match arg.to_string().as_str() {
                            "keep" => on_action(ToastAction::KeepGoing),
                            "stop" => on_action(ToastAction::Stop),
                            _ => {} // "dismiss" / unknown -> treat as no response
                        }
                    }
                }
            }
            Ok(())
        },
    );
    toast.Activated(&handler)?;

    // Prefer the process/package notifier (works under MSIX with package
    // identity); fall back to an explicit-AUMID notifier for unpackaged.
    let notifier = ToastNotificationManager::CreateToastNotifier()
        .or_else(|_| ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)))?;
    notifier.Show(&toast)?;

    // Hold the toast so its Activated handler stays attached.
    if let Ok(mut slot) = LAST_TOAST.lock() {
        *slot = Some(toast);
    }
    tracing::info!(client, engagement, "continuation reminder toast shown");
    Ok(())
}
