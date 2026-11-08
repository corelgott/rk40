//! HTTP API and embedded single-page app.

use crate::device::{Command, DeviceHandle};
use crate::jobs::JobStore;
use crate::planner::{extract_palette, plan_markers, plan_svg, PrepareSettings};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

pub struct AppState {
    pub device: DeviceHandle,
    pub jobs: JobStore,
}

type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(get_status))
        .route("/api/events", get(sse_events))
        .route("/api/device/connect", post(device_connect))
        .route("/api/device/disconnect", post(device_disconnect))
        .route("/api/device/home", post(device_home))
        .route("/api/device/unlock", post(device_unlock))
        .route("/api/device/stop", post(device_stop))
        .route("/api/device/pause", post(device_pause))
        .route("/api/device/move", post(device_move))
        .route("/api/palette", post(palette))
        .route("/api/print", post(print_direct))
        .route("/api/print/markers", post(print_markers))
        .route(
            "/api/workspace",
            get(workspace_get).put(workspace_put).delete(workspace_delete),
        )
        .route("/api/defaults", get(defaults_get).put(defaults_put))
        .route("/api/jobs", get(list_jobs).post(prepare_job))
        .route("/api/jobs/{id}", get(get_job).put(rename_job))
        .route("/api/jobs/{id}", delete(delete_job))
        .route("/api/jobs/{id}/preview.png", get(job_preview))
        .route("/api/jobs/{id}/source.svg", get(job_source))
        .route("/api/jobs/{id}/download.egv", get(job_download_egv))
        .route("/api/jobs/{id}/print", post(print_job))
        // The workspace state carries every SVG on the stage, including all
        // files of a batch; axum's 2 MB default is far too small for that.
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

async fn get_status(State(state): State<SharedState>) -> Response {
    Json(state.device.status_snapshot()).into_response()
}

async fn sse_events(
    State(state): State<SharedState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.device.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(data) => Some(Ok(Event::default().event("status").data(data))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

//////////////////////////////////////////////////////////////////////////////
// Device control
//////////////////////////////////////////////////////////////////////////////

fn send_cmd(state: &SharedState, cmd: Command) -> Response {
    match state.device.send(cmd) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn device_connect(State(state): State<SharedState>) -> Response {
    send_cmd(&state, Command::Connect)
}

async fn device_disconnect(State(state): State<SharedState>) -> Response {
    send_cmd(&state, Command::Disconnect)
}

async fn device_home(State(state): State<SharedState>) -> Response {
    if state.device.status_snapshot().busy {
        return err(StatusCode::CONFLICT, "Ein Job läuft gerade");
    }
    send_cmd(&state, Command::Home)
}

async fn device_unlock(State(state): State<SharedState>) -> Response {
    if state.device.status_snapshot().busy {
        return err(StatusCode::CONFLICT, "Ein Job läuft gerade");
    }
    send_cmd(&state, Command::Unlock)
}

async fn device_stop(State(state): State<SharedState>) -> Response {
    state.device.request_stop();
    // if idle, nothing to interrupt; flag is reset on next job start
    Json(json!({ "ok": true })).into_response()
}

async fn device_pause(State(state): State<SharedState>) -> Response {
    let status = state.device.status_snapshot();
    if status.busy {
        state.device.request_pause_toggle();
        Json(json!({ "ok": true })).into_response()
    } else {
        send_cmd(&state, Command::PauseToggle)
    }
}

#[derive(Deserialize)]
struct MoveReq {
    dx_mm: f64,
    dy_mm: f64,
    /// optional speed limit in mm/s (jog compensation); absent or 0 = full
    /// rapid speed
    #[serde(default)]
    feed_mmps: Option<f64>,
}

async fn device_move(State(state): State<SharedState>, Json(req): Json<MoveReq>) -> Response {
    if state.device.status_snapshot().busy {
        return err(StatusCode::CONFLICT, "Ein Job läuft gerade");
    }
    let dx = (req.dx_mm / 25.4 * 1000.0).round() as i64;
    let dy = (req.dy_mm / 25.4 * 1000.0).round() as i64;
    let feed = req
        .feed_mmps
        .filter(|f| *f > 0.0)
        .map(|f| f.clamp(0.5, 240.0));
    send_cmd(&state, Command::RapidMove(dx, dy, feed))
}

//////////////////////////////////////////////////////////////////////////////
// Jobs
//////////////////////////////////////////////////////////////////////////////

#[derive(Deserialize)]
struct PaletteReq {
    svg: String,
}

/// Extract the distinct colors of an SVG so the UI can offer per-color
/// operation assignment (engrave / vector engrave / cut / ignore).
async fn palette(Json(req): Json<PaletteReq>) -> Response {
    let result = tokio::task::spawn_blocking(move || extract_palette(&req.svg)).await;
    match result {
        Ok(Ok((colors, w_mm, h_mm))) => {
            Json(json!({ "colors": colors, "width_mm": w_mm, "height_mm": h_mm })).into_response()
        }
        Ok(Err(e)) => err(StatusCode::UNPROCESSABLE_ENTITY, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

//////////////////////////////////////////////////////////////////////////////
// Direct printing (no project required)
//////////////////////////////////////////////////////////////////////////////

fn require_idle_connected(state: &SharedState) -> Option<Response> {
    let status = state.device.status_snapshot();
    if status.state != crate::device::ConnState::Connected {
        return Some(err(StatusCode::CONFLICT, "Laser ist nicht verbunden"));
    }
    if status.busy {
        return Some(err(StatusCode::CONFLICT, "Ein Job läuft bereits"));
    }
    None
}

#[derive(Deserialize)]
struct DirectPrintReq {
    #[serde(default)]
    name: Option<String>,
    svg: String,
    settings: PrepareSettings,
}

/// Slice and print in one step, without storing a project.
async fn print_direct(State(state): State<SharedState>, Json(req): Json<DirectPrintReq>) -> Response {
    if let Some(resp) = require_idle_connected(&state) {
        return resp;
    }
    let settings = req.settings.clone();
    let svg = req.svg;
    let planned = tokio::task::spawn_blocking(move || plan_svg(&svg, &settings)).await;
    let planned = match planned {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let name = req.name.unwrap_or_else(|| "Aktuelles Design".to_string());
    match state.device.send(Command::RunJob(name, planned.egv, 1)) {
        Ok(()) => Json(json!({ "ok": true, "estimate_s": planned.estimate_s })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct MarkersReq {
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
    #[serde(default = "default_marker_feed")]
    feed_mmps: f64,
}

fn default_marker_feed() -> f64 {
    100.0
}

/// Laser four small corner markers of the given bounding box, e.g. onto a
/// template sheet, to make positioning the real workpiece easy.
async fn print_markers(State(state): State<SharedState>, Json(req): Json<MarkersReq>) -> Response {
    if let Some(resp) = require_idle_connected(&state) {
        return resp;
    }
    let egv = match plan_markers(req.x_mm, req.y_mm, req.w_mm, req.h_mm, req.feed_mmps) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    match state.device.send(Command::RunJob("Positions-Marker".to_string(), egv, 1)) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

//////////////////////////////////////////////////////////////////////////////
// Workspace (server-side working copy) & defaults
//////////////////////////////////////////////////////////////////////////////

async fn workspace_get(State(state): State<SharedState>) -> Response {
    let (svg, ws_state) = state.jobs.workspace_read();
    Json(json!({ "svg": svg, "state": ws_state })).into_response()
}

#[derive(Deserialize)]
struct WorkspacePut {
    #[serde(default)]
    svg: Option<String>,
    state: serde_json::Value,
}

async fn workspace_put(State(state): State<SharedState>, Json(req): Json<WorkspacePut>) -> Response {
    match state.jobs.workspace_write(req.svg.as_deref(), &req.state) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn workspace_delete(State(state): State<SharedState>) -> Response {
    match state.jobs.workspace_clear() {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn defaults_get(State(state): State<SharedState>) -> Response {
    Json(state.jobs.defaults_read()).into_response()
}

async fn defaults_put(State(state): State<SharedState>, Json(v): Json<serde_json::Value>) -> Response {
    match state.jobs.defaults_write(&v) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn list_jobs(State(state): State<SharedState>) -> Response {
    Json(state.jobs.list()).into_response()
}

#[derive(Deserialize)]
struct PrepareReq {
    name: String,
    svg: String,
    settings: PrepareSettings,
}

async fn prepare_job(State(state): State<SharedState>, Json(req): Json<PrepareReq>) -> Response {
    if req.name.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "Name fehlt");
    }
    // Slicing is CPU-bound: run on a blocking thread
    let settings = req.settings.clone();
    let svg = req.svg.clone();
    let planned = tokio::task::spawn_blocking(move || plan_svg(&svg, &settings)).await;
    let planned = match planned {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    match state.jobs.create(
        req.name.trim(),
        &req.settings,
        &req.svg,
        &planned.egv,
        &planned.preview_png,
        planned.width_mm,
        planned.height_mm,
        planned.estimate_s,
    ) {
        Ok(meta) => (StatusCode::CREATED, Json(meta)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn get_job(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.jobs.get(&id) {
        Some(meta) => Json(meta).into_response(),
        None => err(StatusCode::NOT_FOUND, "Job nicht gefunden"),
    }
}

#[derive(Deserialize)]
struct RenameReq {
    name: String,
}

async fn rename_job(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<RenameReq>,
) -> Response {
    match state.jobs.rename(&id, req.name.trim()) {
        Some(meta) => Json(meta).into_response(),
        None => err(StatusCode::NOT_FOUND, "Job nicht gefunden"),
    }
}

async fn delete_job(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    if state.jobs.delete(&id) {
        Json(json!({ "ok": true })).into_response()
    } else {
        err(StatusCode::NOT_FOUND, "Job nicht gefunden")
    }
}

async fn job_preview(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.jobs.read_preview(&id) {
        Some(png) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        None => err(StatusCode::NOT_FOUND, "Keine Vorschau"),
    }
}

/// Download the precomputed EGV in K40-Whisperer-compatible file format.
async fn job_download_egv(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let meta = match state.jobs.get(&id) {
        Some(m) => m,
        None => return err(StatusCode::NOT_FOUND, "Job nicht gefunden"),
    };
    let egv = match state.jobs.read_egv(&id) {
        Some(d) => d,
        None => return err(StatusCode::NOT_FOUND, "EGV-Daten fehlen"),
    };
    let mut file = Vec::with_capacity(egv.len() + 128);
    file.extend_from_slice(b"Document type : LHYMICRO-GL file\n");
    file.extend_from_slice(b"Creator-Software: rk40\n\n");
    file.extend_from_slice(b"%0%0%0%0%");
    file.extend_from_slice(&egv);
    let fname = format!(
        "attachment; filename=\"{}.egv\"",
        meta.name.replace(['"', '\\', '/', '\n'], "_")
    );
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, fname),
        ],
        file,
    )
        .into_response()
}

async fn job_source(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.jobs.read_source(&id) {
        Some(svg) => ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response(),
        None => err(StatusCode::NOT_FOUND, "Keine Quelle"),
    }
}

#[derive(Deserialize, Default)]
struct PrintReq {
    passes: Option<u32>,
}

async fn print_job(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: Option<Json<PrintReq>>,
) -> Response {
    let status = state.device.status_snapshot();
    if status.state != crate::device::ConnState::Connected {
        return err(StatusCode::CONFLICT, "Laser ist nicht verbunden");
    }
    if status.busy {
        return err(StatusCode::CONFLICT, "Ein Job läuft bereits");
    }
    let meta = match state.jobs.get(&id) {
        Some(m) => m,
        None => return err(StatusCode::NOT_FOUND, "Job nicht gefunden"),
    };
    let egv = match state.jobs.read_egv(&id) {
        Some(d) => d,
        None => return err(StatusCode::NOT_FOUND, "EGV-Daten fehlen"),
    };
    let passes = body
        .and_then(|Json(b)| b.passes)
        .unwrap_or(meta.settings.passes)
        .clamp(1, 100);
    send_cmd(&state, Command::RunJob(meta.name, egv, passes))
}
