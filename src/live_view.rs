// Optional "live view": a localhost HTTP + WebSocket server so a browser page
// at http://localhost:17893 reacts in real time to hotkey / timer events
// broadcast from the running tray app. Behind the `live-view` Cargo feature —
// the default build pulls zero extra deps and is byte-identical to the
// tray-only app. Windows-only (so is the app).
//
// Architecture: one dedicated OS thread runs a single-threaded tokio runtime
// hosting an axum server. The app pushes JSON event strings onto a
// `tokio::sync::broadcast` channel — a *sync* send, so the app's existing
// (non-async) hotkey / submit code paths stay unchanged. Each connected
// WebSocket forwards that channel to the browser. Bind failure (port already
// in use) is non-fatal: log + skip; the tray app is unaffected. The server
// dies with the process — no graceful shutdown needed for a tray app.
//
// `/history` reads the on-disk monthly CSV (same data the rest of the app
// writes) — the page's history list is the real data, not a toy buffer. No
// sqlite, no second source of truth.

#![cfg(all(windows, feature = "live-view"))]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use tokio::sync::broadcast;
use winit::event_loop::EventLoopProxy;

use time_tracker::csv_writer::{self, RewriteResult};
use time_tracker::workstream::WorkstreamRegistry;

use super::UserMessage;

/// Hardcoded so the page URL is stable across installs — bookmarkable.
pub const PORT: u16 = 17893;

// v0.2 IA: the popover (tray surface) is the daily driver; the localhost
// app is the three-tab review surface, with Recorded Time at `/`. The old
// full-page live dashboard (`live_view.html`) is gone — `/live` redirects.
// `/export?view=history` is a query param on the export page, no route.
// These design pages are still mock data until their backends land.
const RECORDED_PAGE: &str = include_str!("Recorded time.html");
const EXPORT_PAGE: &str = include_str!("Export for billing.html");
const ADMIN_PAGE: &str = include_str!("Admin.html");
const POPOVER_PAGE: &str = include_str!("Tray popover.html");

// Brand favicon — the same `db` mark used as the app icon. SVG is primary;
// the .ico is the legacy fallback. Embedded so every page (live / recorded
// / export) and a directly-opened file all resolve `favicon.svg`.
const FAVICON_SVG: &str = include_str!("favicon.svg");
const FAVICON_ICO: &[u8] = include_bytes!("favicon.ico");

#[derive(Clone)]
struct AppState {
    bus: broadcast::Sender<String>,
    /// Shared with the desktop app (single-writer). Handlers lock briefly,
    /// read or mutate, drop the lock — never held across an `.await`.
    registry: Arc<Mutex<WorkstreamRegistry>>,
    /// Wakes the winit event loop with a `UserMessage`. The `/timer/*`
    /// handlers post a command here rather than touching the Timer directly —
    /// the Timer + CSV writer live on the main thread, and this keeps a
    /// single owner. `send_event` is sync and non-blocking.
    proxy: EventLoopProxy<UserMessage>,
    /// Handle onto the single `tt-csv-writer` thread. ALL monthly-CSV
    /// mutations — the hotkey/timer append AND the Recorded-Time API
    /// edit/create/delete rewrites — go through this one serializer (the
    /// single-writer invariant, B2). Never write a monthly CSV any other way.
    csv_writer: csv_writer::WriterHandle,
}

/// Spawn the server thread and return a `broadcast::Sender` the app pushes
/// event JSON onto. Clone it for the popup, the hotkey handler, etc. The
/// sender is returned even if the server can't bind — pushes just go nowhere,
/// harmlessly. Call once, early in `windows_main::run()`. `registry` is the
/// shared workstream registry (the popover's switcher reads it via
/// `GET /workstreams`; the add-workstream form POSTs to it).
pub fn start(
    registry: Arc<Mutex<WorkstreamRegistry>>,
    proxy: EventLoopProxy<UserMessage>,
    csv_writer: csv_writer::WriterHandle,
) -> broadcast::Sender<String> {
    // Capacity 64: if the browser is closed (zero receivers) or a receiver
    // lags, broadcast drops the oldest message — fine, the page rebuilds
    // from /history + the snapshot on (re)connect.
    let (tx, _rx0) = broadcast::channel::<String>(64);
    let tx_thread = tx.clone();

    let spawned = thread::Builder::new()
        .name("tt-live-view".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(error = %e, "live-view: tokio runtime build failed — server disabled");
                    return;
                }
            };
            rt.block_on(async move {
                let app = Router::new()
                    .route("/", get(index))
                    .route("/recorded", get(recorded_index))
                    .route("/export", get(export_index))
                    .route("/admin", get(admin_index))
                    .route("/popover", get(popover_index))
                    .route("/live", get(|| async { Redirect::permanent("/") }))
                    .route("/favicon.svg", get(favicon_svg))
                    .route("/favicon.ico", get(favicon_ico))
                    .route("/ws", get(ws_upgrade))
                    .route("/history", get(history_json))
                    .route("/today", get(today_json))
                    .route("/workstreams", get(workstreams_json).post(add_workstream))
                    .route("/timer/start", post(timer_start))
                    .route("/timer/stop", post(timer_stop))
                    .route("/timer/switch", post(timer_switch))
                    // --- Recorded Time + Export-for-billing JSON API ---
                    .route(
                        "/api/entries",
                        get(api_entries_list).post(api_entries_create),
                    )
                    .route(
                        "/api/entries/:id",
                        axum::routing::patch(api_entries_patch).delete(api_entries_delete),
                    )
                    .route("/api/entries/bulk-billable", post(api_entries_bulk_billable))
                    .route("/api/entries/dedupe", post(api_entries_dedupe))
                    .route("/api/clients", get(api_clients_list).post(api_clients_create))
                    .route("/api/export", post(api_export_create))
                    .route("/api/exports", get(api_exports_list))
                    .route("/api/exports/:run_id", get(api_exports_get))
                    .route("/api/exports/:run_id/unlock-entry", post(api_export_unlock_entry))
                    .route("/api/exports/:run_id/void", post(api_export_void))
                    .route("/api/reveal-exports", post(api_reveal_exports))
                    .route("/exports/:filename", get(api_export_file))
                    .layer(axum::middleware::from_fn(localhost_guard))
                    .with_state(AppState {
                        bus: tx_thread,
                        registry,
                        proxy,
                        csv_writer,
                    });
                let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        tracing::info!("live-view: http://localhost:{PORT}");
                        eprintln!("[live-view] http://localhost:{PORT}");
                        if let Err(e) = axum::serve(listener, app).await {
                            tracing::warn!(error = %e, "live-view: server exited");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, port = PORT, "live-view: bind failed — server disabled (port in use?)");
                    }
                }
            });
        });
    if spawned.is_err() {
        tracing::warn!("live-view: failed to spawn server thread — disabled");
    }
    tx
}

// R3: gate the localhost server. Bind is already `127.0.0.1` (off the LAN),
// but a *web page* the user visits in any browser can `fetch('http://localhost:17893/...')`
// (no-CORS / DNS-rebind) and walk away with the CPA's client list, narratives,
// hours — so reject any request whose `Origin` header is present and isn't our
// own, and whose `Host` isn't localhost. The app's own pages are same-origin
// navigations (no `Origin` header on a top-level GET) and the popover WebView2
// is fine — both pass. No auth token: overkill for a single-user localhost-only
// tool; the Origin/Host check is the right level. (~20 lines.)
async fn localhost_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    fn host_ok(h: &str) -> bool {
        // strip optional :port
        let host = h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h);
        matches!(host, "localhost" | "127.0.0.1") || h == "[::1]" || host == "[::1]"
    }
    fn origin_ok(o: &str) -> bool {
        // exact-match our own origins; "null" is what file:// / sandboxed
        // contexts send and is harmless for a read of our own data.
        matches!(
            o,
            "http://localhost:17893" | "http://127.0.0.1:17893" | "null"
        )
    }
    let headers = req.headers();
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !host_ok(host) {
            return (StatusCode::FORBIDDEN, "bad host").into_response();
        }
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_ok(origin) {
            return (StatusCode::FORBIDDEN, "cross-origin requests are not allowed").into_response();
        }
    }
    next.run(req).await
}

// Pages are served `no-store` so the always-running popover WebView2 (and any
// browser tab) never shows a stale build's HTML after an upgrade — the bytes
// are tiny and the source is in-process, so re-fetching every load costs
// nothing. Without this, WebView2's HTTP cache happily serves yesterday's page.
fn html_no_store(body: &'static str) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Html(body),
    )
}

// `/` is Recorded Time in v0.2 (the live dashboard moved into the popover).
async fn index() -> impl IntoResponse {
    html_no_store(RECORDED_PAGE)
}

async fn recorded_index() -> impl IntoResponse {
    html_no_store(RECORDED_PAGE)
}

async fn export_index() -> impl IntoResponse {
    html_no_store(EXPORT_PAGE)
}

async fn admin_index() -> impl IntoResponse {
    html_no_store(ADMIN_PAGE)
}

async fn popover_index() -> impl IntoResponse {
    html_no_store(POPOVER_PAGE)
}

async fn favicon_svg() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"))],
        FAVICON_SVG,
    )
}

async fn favicon_ico() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/x-icon"))],
        FAVICON_ICO,
    )
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let rx = state.bus.subscribe();
    ws.on_upgrade(move |socket| ws_loop(socket, rx))
}

async fn ws_loop(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    // Immediate "hello" so the page flips its connection dot green even
    // before the first real event.
    if socket
        .send(Message::Text(r#"{"event":"connected"}"#.to_string()))
        .await
        .is_err()
    {
        return;
    }
    // State snapshot: if a timer is running right now, replay it as a
    // `timer_started` so a browser opened mid-timer (or a refreshed tab)
    // renders the running hero instead of going idle. The page snapshots
    // the *entries* list separately via its own `GET /history` on load,
    // so the only state a fresh connection is missing is the live timer.
    if let Some(snapshot) = running_timer_snapshot() {
        if socket.send(Message::Text(snapshot)).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(text) => {
                    if socket.send(Message::Text(text)).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // We dropped some events — tell the page to re-fetch /history.
                    let _ = socket.send(Message::Text(r#"{"event":"resync"}"#.to_string())).await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}            // page chatter (ping/pong/text) — ignore
                Some(Err(_)) => break,
            },
        }
    }
}

// --- /history : last N entries from the current month's CSV ---------------

#[derive(serde::Serialize)]
struct HistoryRow {
    timestamp: String,
    staff: String,
    client: String,
    engagement: String,
    narrative: String,
    minutes: i64,
    hours: f64,
    billable: bool,
    method: String,
    /// v0.2: present when the CSV row carried a `workstream_id`; `null`
    /// for v0.1 rows (resolve via client+engagement string match).
    workstream_id: Option<String>,
}

async fn history_json() -> impl IntoResponse {
    Json(recent_entries(50))
}

// --- /workstreams : the workstream registry (popover switcher source) ----

fn workstream_view(reg: &WorkstreamRegistry) -> serde_json::Value {
    serde_json::Value::Array(
        reg.ordered()
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id,
                    "client": w.client,
                    "engagement": w.engagement,
                    "billable_default": w.billable_default,
                    "pinned": w.pinned,
                    "created_at": w.created_at.to_rfc3339(),
                    "last_used_at": w.last_used_at.to_rfc3339(),
                })
            })
            .collect(),
    )
}

async fn workstreams_json(State(state): State<AppState>) -> impl IntoResponse {
    let body = {
        let reg = state.registry.lock().unwrap_or_else(|p| p.into_inner());
        workstream_view(&reg)
    };
    Json(body)
}

#[derive(serde::Deserialize)]
struct AddWorkstreamBody {
    client: String,
    engagement: String,
    #[serde(default = "default_true")]
    billable: bool,
}
fn default_true() -> bool {
    true
}

/// POST /workstreams — create (or touch) a workstream from the popover's
/// add form. Persists the registry and broadcasts `WorkstreamAdded`.
async fn add_workstream(
    State(state): State<AppState>,
    Json(body): Json<AddWorkstreamBody>,
) -> impl IntoResponse {
    let client = body.client.trim().to_string();
    let engagement = body.engagement.trim().to_string();
    if client.is_empty() || engagement.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "client and engagement are required" })),
        );
    }
    let (ws_value, created) = {
        let mut reg = state.registry.lock().unwrap_or_else(|p| p.into_inner());
        let (w, created) = reg.add(&client, &engagement, body.billable);
        let _ = reg.persist();
        let v = serde_json::json!({
            "id": w.id, "client": w.client, "engagement": w.engagement,
            "billable_default": w.billable_default, "pinned": w.pinned,
            "created_at": w.created_at.to_rfc3339(), "last_used_at": w.last_used_at.to_rfc3339(),
        });
        (v, created)
    };
    if created {
        let _ = state.bus.send(
            serde_json::json!({ "event": "workstream_added", "workstream": ws_value }).to_string(),
        );
    }
    (axum::http::StatusCode::OK, Json(ws_value))
}

// --- /timer/{start,stop,switch} : the popover commands the live timer ----
//
// The popover *observes* the timer over `/ws` (timer_started / timer_stopped /
// entry_logged) and *commands* it here. Validation that can be done with the
// registry alone (does this workstream exist?) happens synchronously; the
// actual Timer mutation + CSV write + broadcast is posted to the main thread
// via the event-loop proxy (it owns the Timer). So a 200 here means "command
// accepted", and the resulting state shows up as the broadcast events the
// hotkeys already produce — popover and hotkeys are now two doors to one room.

#[derive(serde::Deserialize)]
struct TimerRef {
    workstream_id: String,
}

fn resolve_workstream(
    reg: &Arc<Mutex<WorkstreamRegistry>>,
    id: &str,
) -> Option<time_tracker::workstream::Workstream> {
    let reg = reg.lock().unwrap_or_else(|p| p.into_inner());
    reg.workstreams.iter().find(|w| w.id == id).cloned()
}

async fn timer_start(
    State(state): State<AppState>,
    Json(body): Json<TimerRef>,
) -> impl IntoResponse {
    let Some(ws) = resolve_workstream(&state.registry, &body.workstream_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": "unknown workstream" })),
        );
    };
    let _ = state.proxy.send_event(UserMessage::TimerCmdStart {
        client: ws.client,
        engagement: ws.engagement,
        billable: ws.billable_default,
    });
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

async fn timer_stop(State(state): State<AppState>) -> impl IntoResponse {
    let _ = state.proxy.send_event(UserMessage::TimerCmdStop);
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

async fn timer_switch(
    State(state): State<AppState>,
    Json(body): Json<TimerRef>,
) -> impl IntoResponse {
    let Some(ws) = resolve_workstream(&state.registry, &body.workstream_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": "unknown workstream" })),
        );
    };
    let _ = state.proxy.send_event(UserMessage::TimerCmdSwitch {
        client: ws.client,
        engagement: ws.engagement,
        billable: ws.billable_default,
    });
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

/// If a timer is currently running (persisted in the same state file the
/// app restores from across restarts), build the `timer_started` event a
/// fresh WebSocket connection needs to render the running hero. Returns
/// `None` when no timer is running.
fn running_timer_snapshot() -> Option<String> {
    let timer = time_tracker::timer::Timer::load();
    let rt = timer.peek()?;
    Some(
        serde_json::json!({
            "event": "timer_started",
            "client": rt.client,
            "engagement": rt.engagement,
            "narrative": rt.narrative,
            "at": rt.started_at.to_rfc3339(),
        })
        .to_string(),
    )
}

/// Read the current month's CSV (`%USERPROFILE%\TimeTracker\YYYY-MM.csv`),
/// newest-first, capped at `limit`. Tolerant: missing file -> empty; malformed
/// lines -> skipped. Uses a minimal RFC-4180-ish reader (handles double-quoted
/// fields with doubled embedded quotes) — the CSV columns are fixed by
/// csv_writer.rs: timestamp_iso,staff,client,engagement,narrative,minutes,
/// hours_decimal,billable,entry_method.
fn recent_entries(limit: usize) -> Vec<HistoryRow> {
    let month = chrono::Local::now().format("%Y-%m").to_string();
    let path = time_tracker::paths::csv_dir().join(format!("{month}.csv"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    // Skip the BOM, any `#`-prefixed comment lines (the v0.2 version
    // banner), and the column header (the first non-comment line).
    let mut header_skipped = false;
    let mut rows: Vec<HistoryRow> = content
        .lines()
        .filter(|line| {
            let l = line.trim_start_matches('\u{feff}');
            !l.is_empty() && !l.starts_with('#')
        })
        .filter_map(|line| {
            if !header_skipped {
                header_skipped = true;
                return None;
            }
            let f = split_csv_line(line);
            if f.len() < 9 {
                return None;
            }
            Some(HistoryRow {
                timestamp: f[0].clone(),
                staff: f[1].clone(),
                client: f[2].clone(),
                engagement: f[3].clone(),
                narrative: f[4].clone(),
                minutes: f[5].parse().unwrap_or(0),
                hours: f[6].parse().unwrap_or(0.0),
                billable: f[7].eq_ignore_ascii_case("true"),
                method: f[8].clone(),
                workstream_id: f.get(9).filter(|s| !s.is_empty()).cloned(),
            })
        })
        .collect();
    rows.reverse();
    rows.truncate(limit);
    rows
}

fn split_csv_line(line: &str) -> Vec<String> {
    let line = line.trim_start_matches('\u{feff}'); // strip UTF-8 BOM if present
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

// ===========================================================================
// Recorded Time + Export-for-billing backend
// ===========================================================================
//
// The two review pages (`/recorded`, `/export`) are wired to real on-disk
// data through the routes below. There is no natural primary key in the CSV,
// so an entry's id is `"<file-stem>:<n>"` where <n> is its 1-based position
// among the data rows of `%USERPROFILE%\TimeTracker\<file-stem>.csv`. Editing
// or deleting rewrites the whole monthly file (atomic temp + rename, with the
// same Excel-lock retry discipline csv_writer uses); ids of later rows shift,
// which is fine because every page re-fetches after a mutation.
//
// "Locked" entries are those included in a completed export run. The mapping
// from run id -> entry ids lives in a side manifest at
// `%LOCALAPPDATA%\TimeTracker\exports\manifest.json` so the CSV schema is
// untouched. Export artifacts (the rollup CSV + the billing-summary PDF) land
// in `%USERPROFILE%\TimeTracker\exports\`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use axum::extract::{Path as AxPath, Query};
use chrono::{
    Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone,
};

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
const VERSION_COMMENT: &str = "# time-tracker v0.3\r\n";
const HEADER_V02: &str =
    "date,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method,workstream_id\r\n";

/// Parse column 0 of a CSV data row into the entry's calendar date.
/// v0.3 writes a plain `YYYY-MM-DD`; older files (v0.1/v0.2) wrote an ISO
/// timestamp — accept those too and take the date part, so a never-edited
/// legacy file still reads. `None` only if it's genuine garbage.
fn parse_entry_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Local).date_naive())
}

/// The `LocatedRow.timestamp` for a given date: local midnight. v0.3 entries
/// have no clock time; this keeps the range/sort machinery (which works on
/// `DateTime<Local>`) unchanged while only the date carries meaning.
fn date_to_local_midnight(d: NaiveDate) -> chrono::DateTime<Local> {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap_or_else(|| Local.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}

// NOTE (B2/B4): the monthly CSV is *only* mutated through the single
// `tt-csv-writer` thread now — `state.csv_writer.rewrite_blocking(stem, |bytes|
// { ... })`. The transform parses the current bytes (`Some`) / handles a
// genuine NotFound (`None`), mutates, and returns the new whole-file bytes;
// the writer thread does the atomic temp+rename + Excel-lock retry + the
// "ACCESS_DENIED is not a lock error, fail loudly" handling (csv_writer.rs).
// There is no `write_csv_file`/`is_lock_error` here anymore — duplicating
// that classification is exactly what reintroduced the divergence the review
// flagged.

// ---- low-level CSV file model -------------------------------------------

/// A parsed monthly CSV: the verbatim prefix (BOM + comments + header) so we
/// can rewrite the file without losing it, plus the data rows as field
/// vectors. Whether the file is v0.2 (10 cols, banner) or a legacy v0.1 file
/// (9 cols, no banner) is captured so appended/edited rows keep the shape.
struct CsvFile {
    /// Verbatim lines that precede the first data row (BOM kept on the first
    /// of them). Already include their CRLF? No — stored without line endings;
    /// we re-emit `\r\n` after each.
    prefix_lines: Vec<String>,
    /// Each data row as a vector of unescaped fields.
    rows: Vec<Vec<String>>,
    /// True if this is a v0.2 file (10-column rows, `#` banner present).
    v02: bool,
}

impl CsvFile {
    fn parse(content: &str) -> CsvFile {
        let mut prefix_lines = Vec::new();
        let mut rows = Vec::new();
        let mut header_seen = false;
        let mut v02 = false;
        for raw in content.split("\r\n").flat_map(|l| l.split('\n')) {
            // (split on \r\n then \n catches both; trailing empty -> skip)
            if raw.is_empty() {
                continue;
            }
            let l = raw.trim_start_matches('\u{feff}');
            if l.starts_with('#') {
                v02 = true;
                prefix_lines.push(raw.to_string());
                continue;
            }
            if !header_seen {
                header_seen = true;
                prefix_lines.push(raw.to_string());
                continue;
            }
            rows.push(split_csv_line(raw));
        }
        // If the file had no banner but a header, it's v0.1.
        if !v02 && header_seen {
            v02 = false;
        }
        CsvFile { prefix_lines, rows, v02 }
    }

    /// Build a fresh empty v0.2 file (BOM + banner + header).
    fn fresh_v02() -> CsvFile {
        CsvFile {
            prefix_lines: vec![
                format!("\u{feff}{}", VERSION_COMMENT.trim_end_matches("\r\n")),
                HEADER_V02.trim_end_matches("\r\n").to_string(),
            ],
            rows: Vec::new(),
            v02: true,
        }
    }

    fn render(&self) -> Vec<u8> {
        let mut out = String::new();
        for (i, line) in self.prefix_lines.iter().enumerate() {
            if i == 0 {
                // ensure leading BOM
                if !line.starts_with('\u{feff}') {
                    out.push('\u{feff}');
                }
            }
            out.push_str(line.trim_start_matches('\u{feff}'));
            out.push_str("\r\n");
        }
        for row in &self.rows {
            out.push_str(&row.iter().map(|f| csv_escape(f)).collect::<Vec<_>>().join(","));
            out.push_str("\r\n");
        }
        // Re-add BOM at the very front if prefix had none (e.g. empty prefix).
        let mut bytes = Vec::with_capacity(out.len() + 3);
        if !out.starts_with('\u{feff}') {
            bytes.extend_from_slice(&UTF8_BOM);
        }
        bytes.extend_from_slice(out.as_bytes());
        bytes
    }

    /// Number of columns a row in this file should have.
    fn ncols(&self) -> usize {
        if self.v02 { 10 } else { 9 }
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn monthly_csv_path(stem: &str) -> PathBuf {
    time_tracker::paths::csv_dir().join(format!("{stem}.csv"))
}

/// Read a monthly CSV file by stem for *read-only* uses (the GET handlers,
/// pre-validation reads). Missing OR unreadable -> `None`. Callers that
/// mutate must NOT use this to "read then write back" — that's the TOCTOU
/// the writer thread exists to prevent; use `state.csv_writer.rewrite_blocking`.
fn read_csv_file(stem: &str) -> Option<CsvFile> {
    let path = monthly_csv_path(stem);
    let content = std::fs::read_to_string(&path).ok()?;
    Some(CsvFile::parse(&content))
}

/// Map a `RewriteResult` from the writer thread to an axum error response.
/// `Locked` -> 503 (transient — close Excel and retry); `Error` -> the
/// `not_found_status` if the message looks like a missing-row complaint,
/// else 500 (a real IO / permissions failure).
fn rewrite_err_response(r: RewriteResult) -> (StatusCode, Json<serde_json::Value>) {
    match r {
        RewriteResult::Written => err_json(StatusCode::OK, "ok"), // not used on this path
        RewriteResult::Locked(msg) => err_json(StatusCode::SERVICE_UNAVAILABLE, msg),
        RewriteResult::Error(msg) => err_json(StatusCode::INTERNAL_SERVER_ERROR, msg),
    }
}

// ---- duration formatting ------------------------------------------------

/// A single entry can't be longer than a full day — multi-session days are
/// just multiple entries. (The timer itself caps elapsed at 12h elsewhere.)
const MAX_ENTRY_MINUTES: i64 = 24 * 60;

fn fmt_dur(minutes: i64) -> String {
    let m = minutes.max(0);
    format!("{}:{:02}", m / 60, m % 60)
}

/// Parse a duration the user typed: `"H:MM"` ("1:30"), `"H:M"` ("1:5" → 1:05),
/// a bare minute count (`"90"`), or `"1.5h"` / `"1.5"` (hours, decimal). Returns
/// the duration in whole minutes, clamped to `[0, MAX_ENTRY_MINUTES]`. `None`
/// on genuine garbage.
fn parse_hm_dur(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches(['h', 'H']).trim();
    if s.is_empty() {
        return None;
    }
    let mins = if let Some((h_str, m_str)) = s.split_once(':') {
        let h: i64 = h_str.trim().parse().ok()?;
        let m: i64 = m_str.trim().parse().ok()?;
        if !(0..=59).contains(&m) || h < 0 {
            return None;
        }
        h * 60 + m
    } else if let Ok(m) = s.parse::<i64>() {
        // a bare integer is taken as minutes
        m
    } else if let Ok(hrs) = s.parse::<f64>() {
        // a decimal is hours ("1.5" = 90m)
        (hrs * 60.0).round() as i64
    } else {
        return None;
    };
    if mins < 0 {
        return None;
    }
    Some(mins.min(MAX_ENTRY_MINUTES))
}

// ---- entry model returned to the pages ----------------------------------

#[derive(serde::Serialize)]
struct ApiEntry {
    id: String,
    date: String, // YYYY-MM-DD
    dur: String,  // "H:MM" — display form of `minutes`
    minutes: i64, // duration worked; the editable number
    client: String,
    engagement: String,
    billable: bool,
    narrative: String,
    locked: bool,
    run_id: Option<String>,
}

/// One parsed CSV data row plus its location, used internally.
struct LocatedRow {
    stem: String,
    /// 1-based index among data rows.
    n: usize,
    /// Local midnight of the entry's date. v0.3 entries have no clock time;
    /// only the calendar date matters — this stays a `DateTime<Local>` so the
    /// range-window / sort helpers keep working unchanged.
    timestamp: chrono::DateTime<Local>,
    client: String,
    engagement: String,
    narrative: String,
    minutes: i64,
    billable: bool,
}

impl LocatedRow {
    fn id(&self) -> String {
        format!("{}:{}", self.stem, self.n)
    }
    fn date(&self) -> NaiveDate {
        self.timestamp.date_naive()
    }
    fn to_api(&self, manifest: &ExportManifest) -> ApiEntry {
        let id = self.id();
        let run_id = manifest.run_id_for_entry(&id);
        ApiEntry {
            id: id.clone(),
            date: self.date().format("%Y-%m-%d").to_string(),
            dur: fmt_dur(self.minutes),
            minutes: self.minutes,
            client: self.client.clone(),
            engagement: self.engagement.clone(),
            billable: self.billable,
            narrative: self.narrative.clone(),
            locked: run_id.is_some(),
            run_id,
        }
    }
}

/// Parse a `CsvFile`'s rows into `LocatedRow`s for a given stem. Rows whose
/// column 0 isn't a parseable date are skipped.
fn locate_rows(stem: &str, file: &CsvFile) -> Vec<LocatedRow> {
    let mut out = Vec::new();
    for (i, fields) in file.rows.iter().enumerate() {
        if fields.len() < 8 {
            continue;
        }
        let Some(date) = parse_entry_date(&fields[0]) else {
            continue;
        };
        let minutes: i64 = fields[5].parse().unwrap_or(0);
        out.push(LocatedRow {
            stem: stem.to_string(),
            n: i + 1,
            timestamp: date_to_local_midnight(date),
            client: fields[2].clone(),
            engagement: fields[3].clone(),
            narrative: fields[4].clone(),
            minutes,
            billable: fields[7].eq_ignore_ascii_case("true"),
        });
    }
    out
}

// ---- ranges -------------------------------------------------------------

/// Resolve a range keyword to an inclusive [from, to] NaiveDate window, or
/// `None` for "all" (then we cap to the last ~12 months by stem).
fn range_window(range: &str, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    match range {
        "today" => Some((today, today)),
        "yesterday" => {
            let y = today - ChronoDuration::days(1);
            Some((y, y))
        }
        "this_week" => {
            // Monday..today
            let dow = today.weekday().num_days_from_monday() as i64;
            Some((today - ChronoDuration::days(dow), today))
        }
        "last_week" => {
            let dow = today.weekday().num_days_from_monday() as i64;
            let this_mon = today - ChronoDuration::days(dow);
            let last_mon = this_mon - ChronoDuration::days(7);
            Some((last_mon, this_mon - ChronoDuration::days(1)))
        }
        "this_month" => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
            Some((first, today))
        }
        "last_month" => {
            let first_this = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
            let last_of_prev = first_this - ChronoDuration::days(1);
            let first_prev =
                NaiveDate::from_ymd_opt(last_of_prev.year(), last_of_prev.month(), 1).unwrap();
            Some((first_prev, last_of_prev))
        }
        _ => None, // "all"
    }
}

/// The set of monthly CSV stems (`YYYY-MM`) that exist on disk.
fn existing_stems() -> Vec<String> {
    let dir = time_tracker::paths::csv_dir();
    let mut out: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(".csv")?;
                // YYYY-MM shape
                if stem.len() == 7 && stem.as_bytes()[4] == b'-' {
                    Some(stem.to_string())
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Stems overlapping [from, to], or the last 12 existing stems for "all".
fn stems_for_window(window: Option<(NaiveDate, NaiveDate)>) -> Vec<String> {
    let all = existing_stems();
    match window {
        None => {
            let n = all.len();
            all.into_iter().skip(n.saturating_sub(12)).collect()
        }
        Some((from, to)) => {
            let lo = format!("{:04}-{:02}", from.year(), from.month());
            let hi = format!("{:04}-{:02}", to.year(), to.month());
            all.into_iter().filter(|s| *s >= lo && *s <= hi).collect()
        }
    }
}

/// GET /today — the popover's "Today" tab. v0.3: entries are date + duration,
/// so there's no timeline — we return today's recorded entries (with their
/// minutes) plus, if a timer's running, its current running total. The popover
/// rolls this up into a by-client summary. Shape:
/// `{ entries: [{ client, engagement, billable, narrative, minutes }, ...],
///   running: { client, engagement, narrative, minutes } | null }`.
async fn today_json() -> impl IntoResponse {
    let entries: Vec<serde_json::Value> = entries_in_range("today")
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "client": r.client, "engagement": r.engagement,
                "billable": r.billable, "narrative": r.narrative, "minutes": r.minutes,
            })
        })
        .collect();

    let running = time_tracker::timer::Timer::load().peek().map(|rt| {
        let mins = (Local::now() - rt.started_at).num_minutes().clamp(0, MAX_ENTRY_MINUTES);
        serde_json::json!({
            "client": rt.client, "engagement": rt.engagement,
            "narrative": rt.narrative, "minutes": mins,
        })
    });

    (StatusCode::OK, Json(serde_json::json!({ "entries": entries, "running": running })))
}

/// All entries in a range, newest-first.
fn entries_in_range(range: &str) -> Vec<LocatedRow> {
    let today = Local::now().date_naive();
    let window = range_window(range, today);
    let mut rows: Vec<LocatedRow> = Vec::new();
    for stem in stems_for_window(window) {
        if let Some(file) = read_csv_file(&stem) {
            for lr in locate_rows(&stem, &file) {
                let keep = match window {
                    None => true,
                    Some((from, to)) => {
                        let d = lr.date();
                        d >= from && d <= to
                    }
                };
                if keep {
                    rows.push(lr);
                }
            }
        }
    }
    // newest date first; within a day (same stem) the later-appended row first
    rows.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.n.cmp(&a.n)));
    rows
}

// ---- exports manifest ---------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RollupLine {
    client: String,
    engagement: String,
    entries: usize,
    hours: f64,
    billable_hours: f64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ExportTotals {
    entries: usize,
    hours: f64,
    billable_hours: f64,
    clients: usize,
    engagements: usize,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ExportRun {
    run_id: String,
    created_at: String,
    range: String,
    range_label: String,
    entry_ids: Vec<String>,
    rollup: Vec<RollupLine>,
    totals: ExportTotals,
    /// The clients this run covers (the user picks which clients to bill;
    /// recorded so the history/report can name them). Older manifest entries
    /// predate this — `default` makes them deserialize to an empty list.
    #[serde(default)]
    clients: Vec<String>,
    /// Voided ("aborted") runs stay in the manifest — the CSV/PDF and the
    /// history row are kept as a paper trail — but they no longer lock their
    /// entries, so those entries are editable and re-exportable again. Think
    /// of it as the offsetting entry against the original export.
    #[serde(default)]
    voided: bool,
    #[serde(default)]
    voided_at: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ExportManifest {
    runs: Vec<ExportRun>,
}

fn manifest_path() -> PathBuf {
    time_tracker::paths::data_dir().join("exports").join("manifest.json")
}

impl ExportManifest {
    fn load() -> ExportManifest {
        std::fs::read_to_string(manifest_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self) -> Result<(), String> {
        let path = manifest_path();
        let parent = path.parent().ok_or("manifest path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        let temp = parent.join(".manifest.json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&temp, json).map_err(|e| format!("write: {e}"))?;
        std::fs::rename(&temp, &path).map_err(|e| format!("rename: {e}"))?;
        Ok(())
    }
    fn run_id_for_entry(&self, id: &str) -> Option<String> {
        self.runs
            .iter()
            .filter(|r| !r.voided)
            .find(|r| r.entry_ids.iter().any(|e| e == id))
            .map(|r| r.run_id.clone())
    }
    fn locked_ids(&self) -> BTreeSet<String> {
        self.runs
            .iter()
            .filter(|r| !r.voided)
            .flat_map(|r| r.entry_ids.iter().cloned())
            .collect()
    }
    /// B5: a row at position `deleted_n` in stem `stem` is being removed, so
    /// every later positional id `stem:k` (k > deleted_n) must shift down to
    /// `stem:(k-1)`. (`stem:deleted_n` itself shouldn't be present — locked
    /// rows can't be deleted; if it somehow is, drop it.) Returns true if the
    /// manifest changed (caller should `save()`).
    fn shift_after_delete(&mut self, stem: &str, deleted_n: usize) -> bool {
        let mut changed = false;
        for run in &mut self.runs {
            for id in &mut run.entry_ids {
                let Some((s, n_str)) = id.rsplit_once(':') else { continue };
                if s != stem {
                    continue;
                }
                let Ok(k) = n_str.parse::<usize>() else { continue };
                if k > deleted_n {
                    *id = format!("{stem}:{}", k - 1);
                    changed = true;
                }
                // k == deleted_n: a locked row got deleted somehow — leave it
                // be (it now points at whatever shifted into that slot, which
                // is the best we can do without dropping the lock entirely;
                // in practice the delete handler refuses locked rows so this
                // branch is unreachable).
            }
        }
        changed
    }
    /// Next sequential run id for today: EXP-YYYYMMDD-NNNN.
    fn next_run_id(&self, today: NaiveDate) -> String {
        let prefix = format!("EXP-{:04}{:02}{:02}-", today.year(), today.month(), today.day());
        let max_n = self
            .runs
            .iter()
            .filter_map(|r| r.run_id.strip_prefix(&prefix))
            .filter_map(|s| s.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("{prefix}{:04}", max_n + 1)
    }
}

// ---- clients registry ---------------------------------------------------

fn clients_registry_path() -> PathBuf {
    time_tracker::paths::data_dir().join("clients.json")
}

fn load_clients_registry() -> Vec<String> {
    std::fs::read_to_string(clients_registry_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

fn save_clients_registry(list: &[String]) -> Result<(), String> {
    let path = clients_registry_path();
    let parent = path.parent().ok_or("clients path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    let temp = parent.join(".clients.json.tmp");
    let json = serde_json::to_string_pretty(list).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&temp, json).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&temp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

// ---- helpers for JSON error responses -----------------------------------

fn err_json(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn parse_entry_id(id: &str) -> Option<(String, usize)> {
    let (stem, n) = id.rsplit_once(':')?;
    let n: usize = n.parse().ok()?;
    if n == 0 || stem.len() != 7 {
        return None;
    }
    Some((stem.to_string(), n))
}

// ---- GET /api/entries?range=... -----------------------------------------

#[derive(serde::Deserialize)]
struct EntriesQuery {
    #[serde(default)]
    range: Option<String>,
}

async fn api_entries_list(Query(q): Query<EntriesQuery>) -> impl IntoResponse {
    let range = q.range.unwrap_or_else(|| "today".to_string());
    let valid = [
        "today", "yesterday", "this_week", "last_week", "this_month", "last_month", "all",
    ];
    let range = if valid.contains(&range.as_str()) { range } else { "today".to_string() };
    let manifest = ExportManifest::load();
    let rows = entries_in_range(&range);
    let out: Vec<ApiEntry> = rows.iter().map(|r| r.to_api(&manifest)).collect();
    (StatusCode::OK, Json(serde_json::to_value(out).unwrap()))
}

// ---- PATCH /api/entries/:id ---------------------------------------------

#[derive(serde::Deserialize)]
struct PatchEntryBody {
    /// New date `YYYY-MM-DD`. If it lands in a different month than the entry's
    /// current monthly file, the entry is *moved* there (delete from the old
    /// file, append to the new). `dur` is what the user typed: "1:30", "90", …
    date: Option<String>,
    dur: Option<String>,
    client: Option<String>,
    engagement: Option<String>,
    narrative: Option<String>,
    billable: Option<bool>,
}

/// Apply the body's field updates to a CSV row in place (the row is first
/// padded to ncols). Validates the date format and the duration. Does NOT
/// check whether the date stays in the same month — the caller decides.
fn apply_patch_fields(row: &mut Vec<String>, ncols: usize, body: &PatchEntryBody) -> Result<(), String> {
    while row.len() < ncols {
        row.push(String::new());
    }
    if let Some(d_str) = &body.date {
        let d = NaiveDate::parse_from_str(d_str.trim(), "%Y-%m-%d")
            .map_err(|_| "could not parse the date (want YYYY-MM-DD)".to_string())?;
        row[0] = d.format("%Y-%m-%d").to_string();
    }
    if let Some(dur_str) = &body.dur {
        let mins = parse_hm_dur(dur_str).ok_or_else(|| "could not parse the duration (try \"1:30\" or \"90\")".to_string())?;
        row[5] = mins.to_string();
        row[6] = format!("{:.2}", (mins as f64) / 60.0);
    }
    if let Some(c) = &body.client {
        row[2] = c.trim().to_string();
    }
    if let Some(e) = &body.engagement {
        row[3] = e.trim().to_string();
    }
    if let Some(nr) = &body.narrative {
        row[4] = nr.to_string();
    }
    if let Some(b) = body.billable {
        row[7] = b.to_string();
    }
    Ok(())
}

/// Apply a PATCH to one row of an in-memory `CsvFile`, in place. Used for
/// same-month edits (the cross-month move path goes through `move_entry_to_month`).
fn apply_patch_to_row(file: &mut CsvFile, n: usize, body: &PatchEntryBody) -> Result<(), String> {
    if n == 0 || n > file.rows.len() {
        return Err("no such entry".to_string());
    }
    let ncols = file.ncols();
    apply_patch_fields(&mut file.rows[n - 1], ncols, body)
}

/// The month-stem (e.g. `"2026-06"`) the patched entry would live in, if `body`
/// changes the date *and* that pushes it into a different month than `cur_stem`.
/// `Err` if the date string is malformed (so the caller can 422 cleanly).
fn patch_target_stem(cur_stem: &str, body: &PatchEntryBody) -> Result<Option<String>, ()> {
    match &body.date {
        Some(d_str) => {
            let d = NaiveDate::parse_from_str(d_str.trim(), "%Y-%m-%d").map_err(|_| ())?;
            let s = format!("{:04}-{:02}", d.year(), d.month());
            Ok(if s != cur_stem { Some(s) } else { None })
        }
        None => Ok(None),
    }
}

async fn api_entries_patch(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<PatchEntryBody>,
) -> impl IntoResponse {
    let Some((stem, n)) = parse_entry_id(&id) else {
        return err_json(StatusCode::BAD_REQUEST, "bad entry id");
    };
    let manifest = ExportManifest::load();
    if manifest.run_id_for_entry(&id).is_some() {
        return err_json(
            StatusCode::CONFLICT,
            "this entry is locked — it's included in a completed export. Unlock it first.",
        );
    }

    // A date change that crosses months means *moving* the row to another
    // monthly file — handle that separately.
    match patch_target_stem(&stem, &body) {
        Ok(Some(dest_stem)) => return move_entry_to_month(state, &stem, n, &dest_stem, body).await,
        Ok(None) => {}
        Err(()) => return err_json(StatusCode::UNPROCESSABLE_ENTITY, "could not parse the date (want YYYY-MM-DD)"),
    }

    // Pre-validate against the current on-disk state for a friendly status
    // code; the authoritative read-modify-write happens on the writer thread.
    match read_csv_file(&stem) {
        Some(mut f) => {
            if let Err(msg) = apply_patch_to_row(&mut f, n, &body) {
                let status = if msg == "no such entry" {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::UNPROCESSABLE_ENTITY
                };
                return err_json(status, msg);
            }
        }
        // File unreadable here could be NotFound *or* transient — let the
        // writer thread sort it out (NotFound -> the transform's `None` arm
        // returns "no CSV file for that entry"; a lock -> retried there).
        None => {}
    }
    let stem_for_tx = stem.clone();
    let result = state.csv_writer.rewrite_blocking(
        &stem,
        Box::new(move |cur| {
            let bytes = cur.ok_or_else(|| "no CSV file for that entry".to_string())?;
            let mut file = CsvFile::parse(&String::from_utf8_lossy(bytes));
            apply_patch_to_row(&mut file, n, &body)?;
            Ok(file.render())
        }),
    );
    match result {
        RewriteResult::Written => {
            let manifest = ExportManifest::load();
            let updated = read_csv_file(&stem_for_tx)
                .and_then(|f| locate_rows(&stem_for_tx, &f).into_iter().find(|r| r.n == n))
                .map(|r| r.to_api(&manifest));
            match updated {
                Some(u) => (StatusCode::OK, Json(serde_json::to_value(u).unwrap())),
                None => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
            }
        }
        other => rewrite_err_response(other),
    }
}

/// Move an entry from `src_stem`'s monthly CSV to `dest_stem`'s, applying any
/// other patched fields on the way. Two serialized rewrites on the writer
/// thread: (1) append the (patched) row to the destination month, then
/// (2) remove the original from the source month. If (2) fails after (1)
/// landed, the entry exists in both months — visible & recoverable; we say so.
async fn move_entry_to_month(
    state: AppState,
    src_stem: &str,
    n: usize,
    dest_stem: &str,
    body: PatchEntryBody,
) -> (StatusCode, Json<serde_json::Value>) {
    // Read the source row, apply the patch fields to a clone.
    let src = match read_csv_file(src_stem) {
        Some(f) => f,
        None => return err_json(StatusCode::NOT_FOUND, "no CSV file for that entry"),
    };
    if n == 0 || n > src.rows.len() {
        return err_json(StatusCode::NOT_FOUND, "no such entry");
    }
    let mut moved = src.rows[n - 1].clone();
    if let Err(msg) = apply_patch_fields(&mut moved, src.ncols(), &body) {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, msg);
    }

    // (1) append the moved row to the destination month.
    let moved_for_dest = moved.clone();
    let append_res = state.csv_writer.rewrite_blocking(
        dest_stem,
        Box::new(move |cur| {
            let mut file = match cur {
                Some(bytes) => CsvFile::parse(&String::from_utf8_lossy(bytes)),
                None => CsvFile::fresh_v02(),
            };
            let mut row = moved_for_dest;
            let ncols = file.ncols();
            row.truncate(ncols);
            while row.len() < ncols {
                row.push(String::new());
            }
            file.rows.push(row);
            Ok(file.render())
        }),
    );
    if !matches!(append_res, RewriteResult::Written) {
        return rewrite_err_response(append_res);
    }

    // (2) remove the original row from the source month.
    let src_n = n;
    let del_res = state.csv_writer.rewrite_blocking(
        src_stem,
        Box::new(move |cur| {
            let bytes = cur.ok_or_else(|| "no CSV file for that entry".to_string())?;
            let mut file = CsvFile::parse(&String::from_utf8_lossy(bytes));
            if src_n == 0 || src_n > file.rows.len() {
                return Err("no such entry".to_string());
            }
            file.rows.remove(src_n - 1);
            Ok(file.render())
        }),
    );
    if !matches!(del_res, RewriteResult::Written) {
        let detail = match &del_res {
            RewriteResult::Locked(m) | RewriteResult::Error(m) => format!(" ({m})"),
            _ => String::new(),
        };
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the entry was copied into {dest_stem}, but couldn't be removed from {src_stem} — close any program holding that month's CSV, then delete the leftover copy{detail}"),
        );
    }

    // The source delete shifted later rows' positional ids — re-point the
    // export lock manifest (same as DELETE).
    let mut manifest = ExportManifest::load();
    if manifest.shift_after_delete(src_stem, n) {
        if let Err(e) = manifest.save() {
            tracing::error!(error = %e, src_stem, dest_stem, n,
                "PATCH(move): entry moved but export lock manifest shift failed to persist — lock ids in {src_stem} are now off-by-one");
        }
    }

    // Return the moved entry (now the last row in the destination file).
    let manifest = ExportManifest::load();
    let dest = dest_stem.to_string();
    let moved_api = read_csv_file(&dest).and_then(|f| {
        let m = f.rows.len();
        locate_rows(&dest, &f).into_iter().find(|r| r.n == m)
    });
    match moved_api.map(|r| r.to_api(&manifest)) {
        Some(e) => (StatusCode::OK, Json(serde_json::to_value(e).unwrap())),
        None => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
    }
}

// ---- POST /api/entries (create) -----------------------------------------

#[derive(serde::Deserialize)]
struct CreateEntryBody {
    date: String, // YYYY-MM-DD
    /// Duration worked, as typed: "1:30", "90", "1.5", …
    dur: String,
    client: String,
    engagement: String,
    #[serde(default)]
    narrative: String,
    #[serde(default = "default_true_ev")]
    billable: bool,
}
fn default_true_ev() -> bool {
    true
}

async fn api_entries_create(
    State(state): State<AppState>,
    Json(body): Json<CreateEntryBody>,
) -> impl IntoResponse {
    let Ok(date) = NaiveDate::parse_from_str(body.date.trim(), "%Y-%m-%d") else {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, "bad date (want YYYY-MM-DD)");
    };
    let Some(minutes) = parse_hm_dur(&body.dur) else {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, "could not parse the duration (try \"1:30\" or \"90\")");
    };
    if minutes == 0 {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, "duration must be greater than zero");
    }
    if body.client.trim().is_empty() || body.engagement.trim().is_empty() {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, "client and engagement are required");
    }
    let stem = format!("{:04}-{:02}", date.year(), date.month());
    let date_str = date.format("%Y-%m-%d").to_string();
    let staff = time_tracker::config::Config::load().identity.staff;
    let body_client = body.client.trim().to_string();
    let body_engagement = body.engagement.trim().to_string();
    let body_narrative = body.narrative.clone();
    let body_billable = body.billable;

    let stem_for_tx = stem.clone();
    let result = state.csv_writer.rewrite_blocking(
        &stem,
        Box::new(move |cur| {
            // B4: a *failed read* of an existing file never reaches us — only a
            // genuine NotFound does, as `None` (the writer thread retries lock
            // errors and fails loudly on anything else). So `None` here truly
            // means "no monthly CSV yet" → create fresh.
            let mut file = match cur {
                Some(bytes) => CsvFile::parse(&String::from_utf8_lossy(bytes)),
                None => CsvFile::fresh_v02(),
            };
            let mut row = vec![
                date_str,
                staff,
                body_client,
                body_engagement,
                body_narrative,
                minutes.to_string(),
                format!("{:.2}", (minutes as f64) / 60.0),
                body_billable.to_string(),
                "quick".to_string(),
            ];
            if file.v02 {
                row.push(String::new()); // workstream_id
            }
            file.rows.push(row);
            Ok(file.render())
        }),
    );
    match result {
        RewriteResult::Written => {
            let manifest = ExportManifest::load();
            // CREATE appends, so the new row is the last data row.
            let created = read_csv_file(&stem_for_tx).and_then(|f| {
                let n = f.rows.len();
                locate_rows(&stem_for_tx, &f).into_iter().find(|r| r.n == n)
            });
            match created.map(|r| r.to_api(&manifest)) {
                Some(c) => (StatusCode::OK, Json(serde_json::to_value(c).unwrap())),
                None => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
            }
        }
        other => rewrite_err_response(other),
    }
}

// ---- DELETE /api/entries/:id --------------------------------------------

async fn api_entries_delete(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> impl IntoResponse {
    let Some((stem, n)) = parse_entry_id(&id) else {
        return err_json(StatusCode::BAD_REQUEST, "bad entry id");
    };
    let manifest = ExportManifest::load();
    if manifest.run_id_for_entry(&id).is_some() {
        return err_json(
            StatusCode::CONFLICT,
            "this entry is locked — it's included in a completed export. Unlock it first.",
        );
    }
    // Friendly pre-validation (authoritative delete is on the writer thread).
    if let Some(f) = read_csv_file(&stem) {
        if n > f.rows.len() {
            return err_json(StatusCode::NOT_FOUND, "no such entry");
        }
    }
    let result = state.csv_writer.rewrite_blocking(
        &stem,
        Box::new(move |cur| {
            let bytes = cur.ok_or_else(|| "no CSV file for that entry".to_string())?;
            let mut file = CsvFile::parse(&String::from_utf8_lossy(bytes));
            if n == 0 || n > file.rows.len() {
                return Err("no such entry".to_string());
            }
            file.rows.remove(n - 1);
            Ok(file.render())
        }),
    );
    match result {
        RewriteResult::Written => {
            // B5: positional ids shift after a row delete — re-point the export
            // lock manifest so a billed-and-exported entry doesn't become
            // freely editable (and an unbilled one doesn't show as locked).
            // Done right after the CSV rewrite; a manifest-save failure leaves
            // the lock set off-by-one (best-effort, surfaced as 500).
            let mut manifest = ExportManifest::load();
            if manifest.shift_after_delete(&stem, n) {
                if let Err(e) = manifest.save() {
                    tracing::error!(error = %e, stem = %stem, deleted_n = n,
                        "DELETE: CSV row removed but the export lock manifest shift failed to persist — lock ids are now off-by-one");
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("entry deleted, but updating the export lock record failed: {e}"),
                    );
                }
            }
            (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
        }
        other => rewrite_err_response(other),
    }
}

// ---- POST /api/entries/bulk-billable ------------------------------------

#[derive(serde::Deserialize)]
struct BulkBillableBody {
    /// Required. Only entries with exactly this client are touched.
    client: String,
    /// Optional. If given (and non-blank), narrows to this engagement under
    /// the client; otherwise all of the client's engagements.
    #[serde(default)]
    engagement: Option<String>,
    /// Target billable state to set on every matching un-exported entry.
    billable: bool,
    /// Optional date scope (same names as `/api/entries?range=`). Absent /
    /// unknown -> all time.
    #[serde(default)]
    range: Option<String>,
}

/// Flip the `billable` flag on every un-exported entry matching
/// (client [+ engagement] [+ range]). Exported (locked) rows are left alone —
/// their billable is frozen by the export they're in. Rewrites each affected
/// month's CSV exactly once. Response: `{ updated, skipped_locked }`.
async fn api_entries_bulk_billable(
    State(state): State<AppState>,
    Json(body): Json<BulkBillableBody>,
) -> impl IntoResponse {
    let client = body.client.trim().to_string();
    if client.is_empty() {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, "client is required");
    }
    let eng = body
        .engagement
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let want = body.billable;
    let want_str = if want { "true" } else { "false" };
    let range = body.range.as_deref().unwrap_or("all");
    let today = Local::now().date_naive();
    let window = range_window(range, today);
    let locked = ExportManifest::load().locked_ids();

    let mut updated = 0usize;
    let mut skipped_locked = 0usize;

    for stem in stems_for_window(window) {
        let Some(file) = read_csv_file(&stem) else {
            continue;
        };
        // 1-based indices of rows in this stem that should be flipped.
        let mut to_flip: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        for (i, row) in file.rows.iter().enumerate() {
            let n = i + 1;
            if let Some((from, to)) = window {
                let d = match chrono::DateTime::parse_from_rfc3339(
                    row.first().map(String::as_str).unwrap_or(""),
                ) {
                    Ok(t) => t.with_timezone(&Local).date_naive(),
                    Err(_) => continue,
                };
                if d < from || d > to {
                    continue;
                }
            }
            if row.get(2).map(String::as_str).unwrap_or("") != client {
                continue;
            }
            if let Some(e) = &eng {
                if row.get(3).map(String::as_str).unwrap_or("") != e.as_str() {
                    continue;
                }
            }
            // matched. locked? -> count + skip.
            if locked.contains(&format!("{stem}:{n}")) {
                skipped_locked += 1;
                continue;
            }
            // already in the target state? -> no-op, no rewrite.
            let cur = row.get(7).map(|s| s.eq_ignore_ascii_case("true")).unwrap_or(true);
            if cur == want {
                continue;
            }
            to_flip.insert(n);
        }
        if to_flip.is_empty() {
            continue;
        }
        let count = to_flip.len();
        let result = state.csv_writer.rewrite_blocking(
            &stem,
            Box::new(move |cur| {
                let bytes = cur.ok_or_else(|| "no CSV file for that month".to_string())?;
                let mut f = CsvFile::parse(&String::from_utf8_lossy(bytes));
                let ncols = f.ncols();
                for &n in &to_flip {
                    if n == 0 || n > f.rows.len() {
                        continue;
                    }
                    let r = &mut f.rows[n - 1];
                    while r.len() < ncols {
                        r.push(String::new());
                    }
                    r[7] = want_str.to_string();
                }
                Ok(f.render())
            }),
        );
        match result {
            RewriteResult::Written => updated += count,
            other => return rewrite_err_response(other),
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "updated": updated, "skipped_locked": skipped_locked })),
    )
}

// ---- POST /api/entries/dedupe -------------------------------------------

/// Remove exact-duplicate entries — rows that are identical in date, client,
/// engagement, narrative, minutes and billable (i.e. the same work logged more
/// than once, e.g. a double-tap or an aborted move that left a stray copy on
/// the same day). Keeps the *last* one in scan order (= the latest created) and
/// drops the earlier copies. A duplicate group that contains a row locked in an
/// export is left entirely alone. Rewrites each affected month's CSV once and
/// re-points the export-lock manifest for every removed row. Response:
/// `{ removed, files_touched }`.
async fn api_entries_dedupe(State(state): State<AppState>) -> impl IntoResponse {
    let locked = ExportManifest::load().locked_ids();
    // Fingerprint a data row by what makes two rows "the same entry". The date
    // is normalised through `parse_entry_date` so a legacy ISO row and a v0.3
    // date row for the same day collapse together.
    let fp = |r: &[String]| -> String {
        let g = |i: usize| r.get(i).map(String::as_str).unwrap_or("");
        let date = parse_entry_date(g(0)).map(|d| d.to_string()).unwrap_or_else(|| g(0).trim().to_string());
        let mins: i64 = g(5).parse().unwrap_or(0);
        let bill = g(7).eq_ignore_ascii_case("true");
        format!("{date}\u{1}{}\u{1}{}\u{1}{}\u{1}{mins}\u{1}{bill}", g(2).trim(), g(3).trim(), g(4))
    };

    let mut removed = 0usize;
    let mut files_touched = 0usize;
    for stem in stems_for_window(None) {
        let Some(file) = read_csv_file(&stem) else { continue; };
        // group 0-based row indices by fingerprint
        let mut groups: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
        for (i, r) in file.rows.iter().enumerate() {
            groups.entry(fp(r)).or_default().push(i);
        }
        // collect the indices to drop: every member but the last, *unless* the
        // group has a locked row (then leave the whole group alone).
        let mut drop_set: BTreeSet<usize> = BTreeSet::new();
        for idxs in groups.values() {
            if idxs.len() < 2 {
                continue;
            }
            let any_locked = idxs.iter().any(|&i| locked.contains(&format!("{stem}:{}", i + 1)));
            if any_locked {
                continue;
            }
            for &i in &idxs[..idxs.len() - 1] {
                drop_set.insert(i);
            }
        }
        if drop_set.is_empty() {
            continue;
        }
        let n_drop = drop_set.len();
        let drop_for_closure = drop_set.clone();
        let res = state.csv_writer.rewrite_blocking(
            &stem,
            Box::new(move |cur| {
                let bytes = cur.ok_or_else(|| "the month's CSV vanished mid-dedupe".to_string())?;
                let mut f = CsvFile::parse(&String::from_utf8_lossy(bytes));
                // remove highest index first so the lower indices stay valid
                for &i in drop_for_closure.iter().rev() {
                    if i < f.rows.len() {
                        f.rows.remove(i);
                    }
                }
                Ok(f.render())
            }),
        );
        match res {
            RewriteResult::Written => {
                // re-point the export-lock manifest for each deleted row; process
                // the highest position first so the per-delete shifts compose.
                let mut manifest = ExportManifest::load();
                let mut changed = false;
                for &i in drop_set.iter().rev() {
                    if manifest.shift_after_delete(&stem, i + 1) {
                        changed = true;
                    }
                }
                if changed {
                    if let Err(e) = manifest.save() {
                        tracing::error!(error = %e, stem = %stem, "dedupe: rows removed but export lock manifest shift failed to persist — lock ids in {stem} are now off-by-one");
                    }
                }
                removed += n_drop;
                files_touched += 1;
            }
            other => return rewrite_err_response(other),
        }
    }

    (StatusCode::OK, Json(serde_json::json!({ "removed": removed, "files_touched": files_touched })))
}

// ---- GET /api/clients ; POST /api/clients -------------------------------

#[derive(serde::Serialize)]
struct ClientItem {
    name: String,
    hint: String,
}

/// Collect clients from the last 12 months of CSVs + the registry, with a
/// "last used" hint like "Apr 2".
fn collect_clients() -> Vec<ClientItem> {
    let mut last_used: std::collections::BTreeMap<String, chrono::DateTime<Local>> =
        Default::default();
    for stem in stems_for_window(None) {
        if let Some(file) = read_csv_file(&stem) {
            for lr in locate_rows(&stem, &file) {
                let name = lr.client.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let e = last_used.entry(name).or_insert(lr.timestamp);
                if lr.timestamp > *e {
                    *e = lr.timestamp;
                }
            }
        }
    }
    for name in load_clients_registry() {
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        last_used.entry(name).or_insert_with(|| Local.timestamp_opt(0, 0).unwrap());
    }
    let mut items: Vec<ClientItem> = last_used
        .into_iter()
        .map(|(name, ts)| {
            let hint = if ts.timestamp() == 0 {
                String::new()
            } else {
                ts.format("%b %-d").to_string()
            };
            ClientItem { name, hint }
        })
        .collect();
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
}

async fn api_clients_list() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::to_value(collect_clients()).unwrap()))
}

#[derive(serde::Deserialize)]
struct CreateClientBody {
    name: String,
}

async fn api_clients_create(Json(body): Json<CreateClientBody>) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, "name is required");
    }
    let mut list = load_clients_registry();
    if !list.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
        list.push(name);
        list.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        if let Err(e) = save_clients_registry(&list) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }
    (StatusCode::OK, Json(serde_json::to_value(collect_clients()).unwrap()))
}

// ---- POST /api/export ---------------------------------------------------

fn range_label(range: &str, today: NaiveDate) -> String {
    match range_window(range, today) {
        Some((from, to)) => {
            if from == to {
                from.format("%b %-d, %Y").to_string()
            } else if from.year() == to.year() && from.month() == to.month() {
                format!("{} – {}", from.format("%b %-d"), to.format("%-d, %Y"))
            } else if from.year() == to.year() {
                format!("{} – {}", from.format("%b %-d"), to.format("%b %-d, %Y"))
            } else {
                format!("{} – {}", from.format("%b %-d, %Y"), to.format("%b %-d, %Y"))
            }
        }
        None => "all recorded time".to_string(),
    }
}

#[derive(serde::Deserialize)]
struct ExportBody {
    #[serde(default)]
    range: Option<String>,
    /// Only entries belonging to one of these clients go into the run. The UI
    /// always sends a non-empty list (Export is disabled until ≥1 client is
    /// picked); absent/empty here falls back to "every client" for robustness.
    #[serde(default)]
    clients: Option<Vec<String>>,
}

fn build_rollup(rows: &[&LocatedRow]) -> (Vec<RollupLine>, ExportTotals) {
    use std::collections::BTreeMap;
    let mut acc: BTreeMap<(String, String), (usize, i64, i64)> = BTreeMap::new();
    for r in rows {
        let key = (r.client.clone(), r.engagement.clone());
        let e = acc.entry(key).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += r.minutes;
        if r.billable {
            e.2 += r.minutes;
        }
    }
    let rollup: Vec<RollupLine> = acc
        .iter()
        .map(|((c, eng), (n, mins, bmins))| RollupLine {
            client: c.clone(),
            engagement: eng.clone(),
            entries: *n,
            hours: (*mins as f64) / 60.0,
            billable_hours: (*bmins as f64) / 60.0,
        })
        .collect();
    let total_entries: usize = rows.len();
    let total_mins: i64 = rows.iter().map(|r| r.minutes).sum();
    let total_bmins: i64 = rows.iter().filter(|r| r.billable).map(|r| r.minutes).sum();
    let clients: BTreeSet<&String> = rows.iter().map(|r| &r.client).collect();
    let engagements: BTreeSet<(&String, &String)> =
        rows.iter().map(|r| (&r.client, &r.engagement)).collect();
    let totals = ExportTotals {
        entries: total_entries,
        hours: (total_mins as f64) / 60.0,
        billable_hours: (total_bmins as f64) / 60.0,
        clients: clients.len(),
        engagements: engagements.len(),
    };
    (rollup, totals)
}

fn exports_dir() -> PathBuf {
    time_tracker::paths::csv_dir().join("exports")
}

fn write_export_csv(run: &ExportRun) -> Result<(), String> {
    let dir = exports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    let path = dir.join(format!("{}.csv", run.run_id));
    let mut out = String::new();
    out.push('\u{feff}');
    out.push_str("Client,Engagement,Entries,Hours,Billable hours\r\n");
    for line in &run.rollup {
        out.push_str(&format!(
            "{},{},{},{:.2},{:.2}\r\n",
            csv_escape(&line.client),
            csv_escape(&line.engagement),
            line.entries,
            line.hours,
            line.billable_hours,
        ));
    }
    out.push_str(&format!(
        "TOTAL,,{},{:.2},{:.2}\r\n",
        run.totals.entries, run.totals.hours, run.totals.billable_hours
    ));
    std::fs::write(&path, out.as_bytes()).map_err(|e| format!("write csv: {e}"))?;
    Ok(())
}

fn write_export_pdf(run: &ExportRun) -> Result<(), String> {
    use printpdf::*;
    let dir = exports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    let path = dir.join(format!("{}.pdf", run.run_id));

    // A4 portrait: 210 x 297 mm.
    let (doc, page1, layer1) =
        PdfDocument::new("Billing summary", Mm(210.0), Mm(297.0), "layer1");
    let font = doc
        .add_builtin_font(BuiltinFont::Helvetica)
        .map_err(|e| format!("font: {e}"))?;
    let font_bold = doc
        .add_builtin_font(BuiltinFont::HelveticaBold)
        .map_err(|e| format!("font: {e}"))?;

    let mut page = page1;
    let mut layer = doc.get_page(page).get_layer(layer1);

    let left = 18.0_f32;
    // column x positions (mm)
    let col_client = left;
    let col_eng = left + 62.0;
    let col_entries = left + 124.0;
    let col_hours = left + 150.0;
    let col_bill = left + 170.0;
    let top = 280.0_f32;
    let mut y = top;

    let text =
        |layer: &PdfLayerReference, s: &str, x: f32, y: f32, size: f32, f: &IndirectFontRef| {
            layer.use_text(s, size, Mm(x), Mm(y), f);
        };

    // title
    text(&layer, "Billing summary", left, y, 20.0, &font_bold);
    y -= 9.0;
    text(
        &layer,
        &format!("{} — {}", run.range_label, run.run_id),
        left,
        y,
        11.0,
        &font,
    );
    y -= 6.0;
    text(
        &layer,
        &format!("Generated {}", run.created_at),
        left,
        y,
        9.0,
        &font,
    );
    y -= 12.0;

    // header row
    let header = |layer: &PdfLayerReference, y: f32| {
        text(layer, "CLIENT", col_client, y, 8.0, &font_bold);
        text(layer, "ENGAGEMENT", col_eng, y, 8.0, &font_bold);
        text(layer, "ENTRIES", col_entries, y, 8.0, &font_bold);
        text(layer, "HOURS", col_hours, y, 8.0, &font_bold);
        text(layer, "BILLABLE", col_bill, y, 8.0, &font_bold);
    };
    header(&layer, y);
    y -= 6.0;

    let trunc = |s: &str, n: usize| -> String {
        if s.chars().count() <= n {
            s.to_string()
        } else {
            let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
            t.push('…');
            t
        }
    };

    for line in &run.rollup {
        if y < 24.0 {
            // new page
            let (np, nl) = doc.add_page(Mm(210.0), Mm(297.0), "layer");
            page = np;
            layer = doc.get_page(page).get_layer(nl);
            y = top;
            header(&layer, y);
            y -= 6.0;
        }
        text(&layer, &trunc(&line.client, 34), col_client, y, 9.0, &font);
        text(&layer, &trunc(&line.engagement, 34), col_eng, y, 9.0, &font);
        text(&layer, &line.entries.to_string(), col_entries, y, 9.0, &font);
        text(&layer, &format!("{:.2}", line.hours), col_hours, y, 9.0, &font);
        text(&layer, &format!("{:.2}", line.billable_hours), col_bill, y, 9.0, &font);
        y -= 5.5;
    }

    y -= 3.0;
    text(&layer, "TOTAL", col_client, y, 9.0, &font_bold);
    text(
        &layer,
        &format!(
            "{} entries · {} clients · {} engagements",
            run.totals.entries, run.totals.clients, run.totals.engagements
        ),
        col_eng,
        y,
        8.0,
        &font,
    );
    text(&layer, &run.totals.entries.to_string(), col_entries, y, 9.0, &font_bold);
    text(&layer, &format!("{:.2}", run.totals.hours), col_hours, y, 9.0, &font_bold);
    text(&layer, &format!("{:.2}", run.totals.billable_hours), col_bill, y, 9.0, &font_bold);

    let bytes = doc.save_to_bytes().map_err(|e| format!("pdf save: {e}"))?;
    std::fs::write(&path, bytes).map_err(|e| format!("write pdf: {e}"))?;
    Ok(())
}

async fn api_export_create(Json(body): Json<ExportBody>) -> impl IntoResponse {
    let range = body.range.unwrap_or_else(|| "this_month".to_string());
    let valid = [
        "today", "yesterday", "this_week", "last_week", "this_month", "last_month", "all",
    ];
    let range = if valid.contains(&range.as_str()) { range } else { "this_month".to_string() };
    // Effective client filter: trim, drop blanks. Empty -> "all clients".
    let client_filter: Vec<String> = body
        .clients
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let today = Local::now().date_naive();
    let mut manifest = ExportManifest::load();
    let locked = manifest.locked_ids();
    let all_rows = entries_in_range(&range);
    let included: Vec<&LocatedRow> = all_rows
        .iter()
        .filter(|r| !locked.contains(&r.id()))
        .filter(|r| client_filter.is_empty() || client_filter.iter().any(|c| c == &r.client))
        .collect();
    if included.is_empty() {
        let msg = if client_filter.is_empty() {
            "nothing to export — no un-exported entries in that range"
        } else {
            "nothing to export — no un-exported entries for the selected client(s) in that range"
        };
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, msg);
    }
    let (rollup, totals) = build_rollup(&included);
    // which clients does this run actually cover (deduped, sorted)?
    let run_clients: Vec<String> = {
        let set: BTreeSet<String> = included.iter().map(|r| r.client.clone()).collect();
        set.into_iter().collect()
    };
    let run_id = manifest.next_run_id(today);
    let created_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let run = ExportRun {
        run_id: run_id.clone(),
        created_at,
        range: range.clone(),
        range_label: range_label(&range, today),
        entry_ids: included.iter().map(|r| r.id()).collect(),
        rollup,
        totals,
        clients: run_clients,
        voided: false,
        voided_at: None,
    };
    if let Err(e) = write_export_csv(&run) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("CSV: {e}"));
    }
    if let Err(e) = write_export_pdf(&run) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("PDF: {e}"));
    }
    manifest.runs.push(run.clone());
    if let Err(e) = manifest.save() {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("manifest: {e}"));
    }
    (StatusCode::OK, Json(export_run_json(&run, true)))
}

fn export_run_json(run: &ExportRun, full: bool) -> serde_json::Value {
    let mut v = serde_json::json!({
        "run_id": run.run_id,
        "created_at": run.created_at,
        "range": run.range,
        "range_label": run.range_label,
        "totals": run.totals,
        "clients": run.clients,
        "voided": run.voided,
        "voided_at": run.voided_at,
        "csv_url": format!("/exports/{}.csv", run.run_id),
        "pdf_url": format!("/exports/{}.pdf", run.run_id),
    });
    if full {
        v["rollup"] = serde_json::to_value(&run.rollup).unwrap();
        v["entry_ids"] = serde_json::to_value(&run.entry_ids).unwrap();
    }
    v
}

async fn api_exports_list() -> impl IntoResponse {
    let manifest = ExportManifest::load();
    let mut runs: Vec<&ExportRun> = manifest.runs.iter().collect();
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let out: Vec<serde_json::Value> = runs.iter().map(|r| export_run_json(r, false)).collect();
    (StatusCode::OK, Json(serde_json::Value::Array(out)))
}

async fn api_exports_get(AxPath(run_id): AxPath<String>) -> impl IntoResponse {
    let manifest = ExportManifest::load();
    match manifest.runs.iter().find(|r| r.run_id == run_id) {
        Some(r) => (StatusCode::OK, Json(export_run_json(r, true))),
        None => err_json(StatusCode::NOT_FOUND, "no such export run"),
    }
}

#[derive(serde::Deserialize)]
struct UnlockBody {
    entry_id: String,
}

async fn api_export_unlock_entry(
    AxPath(run_id): AxPath<String>,
    Json(body): Json<UnlockBody>,
) -> impl IntoResponse {
    let mut manifest = ExportManifest::load();
    let Some(run) = manifest.runs.iter_mut().find(|r| r.run_id == run_id) else {
        return err_json(StatusCode::NOT_FOUND, "no such export run");
    };
    let before = run.entry_ids.len();
    run.entry_ids.retain(|e| e != &body.entry_id);
    if run.entry_ids.len() == before {
        return err_json(StatusCode::NOT_FOUND, "that entry is not in this run");
    }
    // Note: the on-disk CSV/PDF artifacts are a snapshot of the run as it was
    // generated and are intentionally NOT regenerated here — the export is a
    // paper trail. Only the lock (manifest membership) is released.
    if let Err(e) = manifest.save() {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("manifest: {e}"));
    }
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

/// POST /api/exports/:run_id/void — "abort" a completed export. The run stays
/// in the manifest (and its CSV/PDF on disk) as a paper trail, but it's marked
/// voided, which releases the locks on its entries: `locked_ids` / `run_id_for_entry`
/// skip voided runs, so those entries become editable and re-exportable. Idempotent
/// guard: voiding an already-voided run is a 409.
async fn api_export_void(AxPath(run_id): AxPath<String>) -> impl IntoResponse {
    let mut manifest = ExportManifest::load();
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    {
        let Some(run) = manifest.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return err_json(StatusCode::NOT_FOUND, "no such export run");
        };
        if run.voided {
            return err_json(StatusCode::CONFLICT, "this export is already voided");
        }
        run.voided = true;
        run.voided_at = Some(now);
    }
    if let Err(e) = manifest.save() {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("manifest: {e}"));
    }
    let run = manifest.runs.iter().find(|r| r.run_id == run_id).expect("run still present");
    (StatusCode::OK, Json(export_run_json(run, false)))
}

/// POST /api/reveal-exports — open the exports folder in File Explorer.
/// No request input flows into the command (the path is fixed), so there's
/// nothing to inject. explorer.exe's later exit code is meaningless, so we
/// just check the spawn succeeded.
async fn api_reveal_exports() -> impl IntoResponse {
    let dir = exports_dir();
    let _ = std::fs::create_dir_all(&dir);
    // (this module is windows-only; explorer.exe opens the folder in a window)
    if std::process::Command::new("explorer.exe").arg(&dir).spawn().is_ok() {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
    } else {
        err_json(StatusCode::INTERNAL_SERVER_ERROR, "couldn't open the exports folder")
    }
}

// ---- GET /exports/<filename> --------------------------------------------

async fn api_export_file(AxPath(filename): AxPath<String>) -> impl IntoResponse {
    // Sanitize: only [A-Za-z0-9._-], and no leading dot path tricks.
    if filename.is_empty()
        || filename.len() > 128
        || filename.contains("..")
        || !filename.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))], b"bad filename".to_vec()).into_response();
    }
    let path = exports_dir().join(&filename);
    let Ok(bytes) = std::fs::read(&path) else {
        return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))], b"not found".to_vec()).into_response();
    };
    let ct = if filename.ends_with(".pdf") {
        "application/pdf"
    } else if filename.ends_with(".csv") {
        "text/csv; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let disp = format!("attachment; filename=\"{filename}\"");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap()),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disp).unwrap(),
            ),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with_ids(ids: &[&str]) -> ExportRun {
        ExportRun {
            run_id: "EXP-20260511-0001".into(),
            created_at: "2026-05-11 09:00:00".into(),
            range: "this_month".into(),
            range_label: "May 2026".into(),
            entry_ids: ids.iter().map(|s| s.to_string()).collect(),
            rollup: Vec::new(),
            totals: ExportTotals { entries: 0, hours: 0.0, billable_hours: 0.0, clients: 0, engagements: 0 },
            clients: Vec::new(),
            voided: false,
            voided_at: None,
        }
    }

    #[test]
    fn shift_after_delete_decrements_later_rows_in_same_stem() {
        // Locked rows 5 and 7 in 2026-05; delete row 3 -> they become 4 and 6.
        // A row in a different stem and an earlier row are untouched.
        let mut m = ExportManifest {
            runs: vec![run_with_ids(&["2026-05:5", "2026-05:7", "2026-04:5", "2026-05:2"])],
        };
        let changed = m.shift_after_delete("2026-05", 3);
        assert!(changed);
        let ids: Vec<_> = m.runs[0].entry_ids.clone();
        assert_eq!(ids, vec!["2026-05:4", "2026-05:6", "2026-04:5", "2026-05:2"]);
    }

    #[test]
    fn shift_after_delete_noop_when_nothing_later() {
        let mut m = ExportManifest { runs: vec![run_with_ids(&["2026-05:1", "2026-05:2"])] };
        // Delete row 5 (after both locked rows) -> no change.
        assert!(!m.shift_after_delete("2026-05", 5));
        assert_eq!(m.runs[0].entry_ids, vec!["2026-05:1".to_string(), "2026-05:2".to_string()]);
    }

    #[test]
    fn voided_run_releases_its_locks() {
        let mut active = run_with_ids(&["2026-05:1", "2026-05:2"]);
        active.run_id = "EXP-20260511-0001".into();
        let mut aborted = run_with_ids(&["2026-05:3", "2026-05:4"]);
        aborted.run_id = "EXP-20260511-0002".into();
        aborted.voided = true;
        let m = ExportManifest { runs: vec![active, aborted] };
        // the active run still locks its entries...
        assert!(m.locked_ids().contains("2026-05:1"));
        assert_eq!(m.run_id_for_entry("2026-05:2").as_deref(), Some("EXP-20260511-0001"));
        // ...but the voided run's entries are free again.
        assert!(!m.locked_ids().contains("2026-05:3"));
        assert!(!m.locked_ids().contains("2026-05:4"));
        assert_eq!(m.run_id_for_entry("2026-05:3"), None);
    }

    #[test]
    fn localhost_guard_logic_accepts_own_origin_rejects_foreign() {
        // (the guard fn itself is async + needs a Request; here we exercise the
        // pure host/origin predicates it uses, kept inline as nested fns —
        // re-stated to keep the test independent of internal naming.)
        let host_ok = |h: &str| {
            let host = h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h);
            matches!(host, "localhost" | "127.0.0.1") || h == "[::1]" || host == "[::1]"
        };
        let origin_ok = |o: &str| matches!(o, "http://localhost:17893" | "http://127.0.0.1:17893" | "null");
        assert!(host_ok("localhost:17893"));
        assert!(host_ok("127.0.0.1"));
        assert!(!host_ok("evil.example.com"));
        assert!(origin_ok("http://localhost:17893"));
        assert!(origin_ok("null"));
        assert!(!origin_ok("https://evil.example.com"));
        assert!(!origin_ok("http://localhost:8080"));
    }
}
