use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, body::Body};
use serde::{Deserialize, Serialize};
use serde_json::json;
use weave_core::HopStatus;
use weave_core::webhook::{Event, EventType};

use crate::app::App;
use crate::invite;
use crate::seat::Seat;
use crate::snapshot::{SeatView, Step};
use crate::weave::Proxied;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/", get(operator_page))
        .route("/j/{token}", get(guest_page))
        .route("/assets/{file}", get(asset))
        .route("/api/seats", get(seats))
        .route("/api/seats/{seat}", delete(revoke))
        .route("/api/invites", post(create_invite))
        .route("/api/session/{token}", get(session))
        .route("/api/session/{token}/register", post(register))
        .route("/api/session/{token}/heartbeat", post(heartbeat))
        .route("/api/session/{token}/desired", get(desired))
        .route("/api/weave/events", post(weave_event))
        .with_state(app)
}

// --- pages ---

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn operator_page(State(app): State<Arc<App>>) -> Result<Response, ApiError> {
    page(&app, "operator.html").await
}

/// The token is not read here: the page asks for its own session and gets the
/// verdict from the API, so an expired link renders the same shell either way.
async fn guest_page(State(app): State<Arc<App>>) -> Result<Response, ApiError> {
    page(&app, "guest.html").await
}

async fn asset(
    State(app): State<Arc<App>>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    page(&app, &file).await
}

async fn page(app: &App, name: &str) -> Result<Response, ApiError> {
    let asset = crate::assets::load(name, app.cfg.assets.as_deref())
        .await
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such page"))?;
    Ok((
        [(header::CONTENT_TYPE, asset.content_type)],
        asset.body,
    )
        .into_response())
}

// --- operator ---

#[derive(Serialize)]
struct SeatsResponse {
    target: Target,
    open_live: Option<String>,
    reconciled_at: String,
    errors: Vec<String>,
    seats: Vec<SeatView>,
}

#[derive(Serialize)]
struct Target {
    node: String,
    network: Option<String>,
}

async fn seats(State(app): State<Arc<App>>) -> Json<SeatsResponse> {
    let now = crate::now();
    let seats = app.seats.lock().expect("seat store lock").list();
    let snapshot = app.snapshot.read().expect("snapshot lock");
    Json(SeatsResponse {
        target: Target {
            node: app.cfg.target_node.clone(),
            network: app.cfg.target_network.clone(),
        },
        open_live: app.cfg.open_live_url.clone(),
        reconciled_at: snapshot.reconciled_at.clone(),
        errors: snapshot.errors.clone(),
        seats: seats
            .iter()
            .map(|seat| SeatView::build(seat, &snapshot, &app.cfg, now))
            .collect(),
    })
}

#[derive(Deserialize)]
struct InviteRequest {
    name: String,
    ttl_secs: Option<i64>,
}

async fn create_invite(
    State(app): State<Arc<App>>,
    Json(request): Json<InviteRequest>,
) -> Result<(StatusCode, Json<SeatView>), ApiError> {
    let display_name = request.name.trim();
    if display_name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "a guest needs a name",
        ));
    }
    if display_name.chars().count() > 64 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "that name is too long",
        ));
    }
    let ttl = request.ttl_secs.unwrap_or(app.cfg.invite_ttl_secs);
    if !(60..=7 * 24 * 3600).contains(&ttl) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "a link lasts between a minute and a week",
        ));
    }

    let now = crate::now();
    let suffix = invite::random_suffix().map_err(ApiError::internal)?;
    let id = invite::seat_id(display_name, &suffix);
    let expires_at = now + ttl;
    let seat = Seat {
        token: app.cfg.signing_key.mint(&id, expires_at),
        seat: id,
        display_name: display_name.to_string(),
        created_at: now,
        expires_at,
        revoked: false,
        joined_at: None,
    };
    app.seats
        .lock()
        .expect("seat store lock")
        .insert(seat.clone())
        .map_err(ApiError::internal)?;
    tracing::info!(seat = %seat.seat, "invited a guest");

    let snapshot = app.snapshot.read().expect("snapshot lock");
    Ok((
        StatusCode::CREATED,
        Json(SeatView::build(&seat, &snapshot, &app.cfg, now)),
    ))
}

/// Revoking marks the seat rather than deleting it, so the reconcile loop is
/// still the one thing that withdraws the stream and the seat only disappears
/// once weave has confirmed it is gone.
async fn revoke(
    State(app): State<Arc<App>>,
    Path(seat): Path<String>,
) -> Result<StatusCode, ApiError> {
    let revoked = app
        .seats
        .lock()
        .expect("seat store lock")
        .update(&seat, |seat| {
            let changed = !seat.revoked;
            seat.revoked = true;
            changed
        })
        .map_err(ApiError::internal)?;
    if !revoked && app.seats.lock().expect("seat store lock").get(&seat).is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "no such seat"));
    }
    tracing::info!(%seat, "revoked a seat");
    app.nudge.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

// --- guest session ---

#[derive(Serialize)]
struct SessionResponse {
    seat: String,
    display_name: String,
    expires_at: String,
    stage: &'static str,
    chain: Vec<Step>,
}

async fn session(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
) -> Result<Json<SessionResponse>, ApiError> {
    let now = crate::now();
    let seat = seat_for(&app, &token, now)?;
    let snapshot = app.snapshot.read().expect("snapshot lock");
    let view = SeatView::build(&seat, &snapshot, &app.cfg, now);
    Ok(Json(SessionResponse {
        seat: view.seat,
        display_name: view.display_name,
        expires_at: view.expires_at,
        stage: view.stage,
        chain: view.chain,
    }))
}

#[derive(Deserialize)]
struct Report {
    #[serde(default)]
    hop_status: Vec<HopStatus>,
}

/// The page says how its hops are doing; the gateway decides everything else
/// about the node, including its id. A guest holds no weave credential and
/// cannot register as another seat.
async fn register(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
    Json(report): Json<Report>,
) -> Result<Response, ApiError> {
    let seat = seat_for(&app, &token, crate::now())?;
    record(&app, &seat.seat, report.hop_status.clone());
    let proxied = app
        .weave
        .register(&seat.seat, report.hop_status)
        .await
        .map_err(ApiError::bad_gateway)?;
    if proxied.status.is_success() {
        // The webhook normally beats this, but a demo with no webhook
        // configured still routes the guest.
        app.nudge.notify_one();
    }
    Ok(forward(proxied))
}

async fn heartbeat(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
    Json(report): Json<Report>,
) -> Result<Response, ApiError> {
    let seat = seat_for(&app, &token, crate::now())?;
    record(&app, &seat.seat, report.hop_status.clone());
    let proxied = app
        .weave
        .heartbeat(&seat.seat, report.hop_status)
        .await
        .map_err(ApiError::bad_gateway)?;
    Ok(forward(proxied))
}

async fn desired(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let seat = seat_for(&app, &token, crate::now())?;
    let proxied = app
        .weave
        .desired(&seat.seat)
        .await
        .map_err(ApiError::bad_gateway)?;
    Ok(forward(proxied))
}

fn record(app: &App, seat: &str, hop_status: Vec<HopStatus>) {
    app.snapshot
        .write()
        .expect("snapshot lock")
        .reported
        .insert(seat.to_string(), hop_status);
}

/// A signed token proves which seat the holder was given; the store decides
/// whether that seat is still usable.
fn seat_for(app: &App, token: &str, now: i64) -> Result<Seat, ApiError> {
    let invite = app
        .cfg
        .signing_key
        .verify(token)
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "unknown invite"))?;
    let seat = app
        .seats
        .lock()
        .expect("seat store lock")
        .get(&invite.seat)
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::GONE, "invite revoked"))?;
    if seat.revoked {
        return Err(ApiError::new(StatusCode::GONE, "invite revoked"));
    }
    if seat.expired(now) {
        return Err(ApiError::new(StatusCode::GONE, "invite expired"));
    }
    Ok(seat)
}

// --- weave webhook ---

/// Deliveries are hints. Acting on one means nudging the reconcile loop, which
/// reads the real state itself, so a duplicate, a stale or a lost event all
/// converge to the same place.
async fn weave_event(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(event): Json<Event>,
) -> Result<StatusCode, ApiError> {
    if let Some(expected) = &app.cfg.webhook_token {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token",
            ));
        }
    }

    let known = app
        .seats
        .lock()
        .expect("seat store lock")
        .get(&event.node.id)
        .is_some();
    if known {
        tracing::info!(
            seat = %event.node.id,
            event = %event.event_type,
            "weave reported a lifecycle change"
        );
        app.nudge.notify_one();
    } else {
        // Another node's event is not this gateway's business, and answering
        // 4xx would make the controller stop retrying a delivery it got right.
        tracing::debug!(node = %event.node.id, event = %event.event_type, "not a seat here");
    }
    let _ = EventType::ALL;
    Ok(StatusCode::ACCEPTED)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// --- plumbing ---

fn forward(proxied: Proxied) -> Response {
    let mut response = Response::builder().status(proxied.status);
    if let Some(content_type) = proxied
        .content_type
        .as_deref()
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    response
        .body(Body::from(proxied.body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        tracing::error!("{error:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "the gateway failed")
    }

    fn bad_gateway(error: anyhow::Error) -> Self {
        tracing::warn!("{error:#}");
        Self::new(StatusCode::BAD_GATEWAY, format!("{error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
