use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use axum::{
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use dashmap::DashMap;
use futures_util::StreamExt;
use mongodb::{
    bson::{
        doc,
        spec::BinarySubtype,
        Binary,
        Bson,
        DateTime as BsonDateTime,
        Document,
    },
    options::{ClientOptions, FindOptions, IndexOptions, UpdateOptions},
    Client,
    Collection,
    IndexModel,
};
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use uuid::Uuid;

const MAX_MESSAGE_BLOB_BYTES: usize = 64 * 1024;
const MAX_PREKEY_BUNDLE_BYTES: usize = 16 * 1024;
const MAX_WS_FRAME_BYTES: usize = 96 * 1024;
const DEFAULT_FETCH_LIMIT: u16 = 128;
const MAX_FETCH_LIMIT: u16 = 1024;

const MAX_MESSAGE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const PREKEY_TTL_SECS: i64 = 7 * 24 * 60 * 60;

const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_CONCURRENT_FETCH_PER_IP: usize = 4;

const DEFAULT_JITTER_MIN_MS: u64 = 5;
const DEFAULT_JITTER_MAX_MS: u64 = 25;

#[derive(Clone)]
struct AppState {
    mailbox_messages: Collection<Document>,
    mailbox_activity: Collection<Document>,
    prekeys: Collection<Document>,
    rate_limits: Arc<IpRateLimiter>,
    fetch_limits: Arc<DashMap<IpAddr, Arc<Semaphore>>>,
    metrics: Arc<Metrics>,
    config: Arc<Config>,
}

#[derive(Default)]
struct IpRateLimiter {
    put_message: DashMap<IpAddr, TokenBucket>,
    upload_prekey: DashMap<IpAddr, TokenBucket>,
}

#[derive(Clone, Copy)]
struct TokenBucket {
    tokens: u32,
    last_refill: i64,
}

impl TokenBucket {
    fn new_full(capacity: u32, now: i64) -> Self {
        Self {
            tokens: capacity,
            last_refill: now,
        }
    }

    fn allow(&mut self, capacity: u32, refill_per_minute: u32, now: i64) -> bool {
        let elapsed = now.saturating_sub(self.last_refill);
        if elapsed > 0 {
            // Refill in whole-token increments.
            let refill = (elapsed as u64)
                .saturating_mul(refill_per_minute as u64)
                .saturating_div(60) as u32;
            if refill > 0 {
                self.tokens = (self.tokens + refill).min(capacity);
                self.last_refill = now;
            }
        }

        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

#[derive(Default)]
struct Metrics {
    active_connections: AtomicUsize,
    ws_connections_total: AtomicU64,
    ws_rejected_connections_total: AtomicU64,
    requests_total: AtomicU64,
    rate_limited_total: AtomicU64,
    db_errors_total: AtomicU64,
}

struct Config {
    max_connections: usize,
    idle_timeout: Duration,
    max_concurrent_fetch_per_ip: usize,
    jitter_min: Duration,
    jitter_max: Duration,
}

impl Config {
    fn from_env() -> Self {
        let max_connections = std::env::var("MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);

        let idle_timeout_secs = std::env::var("IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);

        let max_concurrent_fetch_per_ip = std::env::var("MAX_CONCURRENT_FETCH_PER_IP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONCURRENT_FETCH_PER_IP);

        let jitter_min_ms = std::env::var("RESPONSE_JITTER_MIN_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_JITTER_MIN_MS);
        let jitter_max_ms = std::env::var("RESPONSE_JITTER_MAX_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_JITTER_MAX_MS);

        let (jitter_min_ms, jitter_max_ms) = if jitter_min_ms <= jitter_max_ms {
            (jitter_min_ms, jitter_max_ms)
        } else {
            (jitter_max_ms, jitter_min_ms)
        };

        Self {
            max_connections,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            max_concurrent_fetch_per_ip,
            jitter_min: Duration::from_millis(jitter_min_ms),
            jitter_max: Duration::from_millis(jitter_max_ms),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum ClientRequest {
    PutMessage {
        mailbox_id: [u8; 32],
        message_id: [u8; 16],
        ciphertext: Vec<u8>,
        ttl: i64,
    },
    FetchMessages {
        mailbox_id: [u8; 32],
        limit: u16,
    },
    DeleteMessages {
        mailbox_id: [u8; 32],
        message_ids: Vec<[u8; 16]>,
    },
    UploadPrekey {
        bundle_id: [u8; 16],
        payload: Vec<u8>,
    },
    FetchPrekey {
        bundle_id: [u8; 16],
    },
}

#[derive(Serialize, Deserialize, Debug)]
enum ServerResponse {
    Ok,
    Error { code: ErrorCode },
    Messages { messages: Vec<FetchedMessage> },
    Prekey { payload: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
enum ErrorCode {
    BadRequest,
    TooLarge,
    RateLimited,
    NotFound,
    Internal,
}

#[derive(Serialize, Deserialize, Debug)]
struct FetchedMessage {
    message_id: [u8; 16],
    ciphertext: Vec<u8>,
    timestamp: i64,
    ttl: i64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string())
        .parse()
        .context("BIND_ADDR must be a valid SocketAddr")?;

    let mongo_uri = std::env::var("MONGODB_URI").context("MONGODB_URI is required")?;
    let db_name = std::env::var("DB_NAME").unwrap_or_else(|_| "social".to_string());

    let client_options = ClientOptions::parse(&mongo_uri).await?;
    let client = Client::with_options(client_options)?;
    let db = client.database(&db_name);
    let mailbox_messages = db.collection::<Document>("mailbox_messages");
    let mailbox_activity = db.collection::<Document>("mailbox_activity");
    let prekeys = db.collection::<Document>("prekey_bundles");

    ensure_indexes(&mailbox_messages, &mailbox_activity, &prekeys).await?;

    let config = Arc::new(Config::from_env());
    let metrics_state = Arc::new(Metrics::default());

    let state = AppState {
        mailbox_messages,
        mailbox_activity,
        prekeys,
        rate_limits: Arc::new(IpRateLimiter::default()),
        fetch_limits: Arc::new(DashMap::new()),
        metrics: metrics_state,
        config,
    };

    spawn_garbage_collector(state.clone());

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/ws", get(ws_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    info!("listening on {}", bind_addr);
    axum::serve(
        tokio::net::TcpListener::bind(bind_addr).await?,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

async fn healthz() -> impl IntoResponse {
    StatusCode::OK
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let active = state.metrics.active_connections.load(Ordering::Relaxed);
    let ws_total = state.metrics.ws_connections_total.load(Ordering::Relaxed);
    let ws_rejected = state
        .metrics
        .ws_rejected_connections_total
        .load(Ordering::Relaxed);
    let req_total = state.metrics.requests_total.load(Ordering::Relaxed);
    let rl_total = state.metrics.rate_limited_total.load(Ordering::Relaxed);
    let db_err = state.metrics.db_errors_total.load(Ordering::Relaxed);

    // Prometheus text format, no sensitive labels.
    let body = format!(
        "# TYPE social_active_connections gauge\n\
social_active_connections {active}\n\
# TYPE social_ws_connections_total counter\n\
social_ws_connections_total {ws_total}\n\
# TYPE social_ws_rejected_connections_total counter\n\
social_ws_rejected_connections_total {ws_rejected}\n\
# TYPE social_requests_total counter\n\
social_requests_total {req_total}\n\
# TYPE social_rate_limited_total counter\n\
social_rate_limited_total {rl_total}\n\
# TYPE social_db_errors_total counter\n\
social_db_errors_total {db_err}\n"
    );

    (StatusCode::OK, body)
}

async fn ws_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let conn_id = Uuid::new_v4();
    // Enforce global connection cap at accept time.
    let current = state
        .metrics
        .active_connections
        .fetch_add(1, Ordering::Relaxed)
        + 1;
    if current > state.config.max_connections {
        state
            .metrics
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
        state
            .metrics
            .ws_rejected_connections_total
            .fetch_add(1, Ordering::Relaxed);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    state
        .metrics
        .ws_connections_total
        .fetch_add(1, Ordering::Relaxed);

    // No mailbox/message IDs ever logged.
    info!(%conn_id, "ws connected");

    ws.on_upgrade(move |socket| async move {
        handle_socket(socket, peer.ip(), conn_id, state).await;
    })
}

async fn handle_socket(mut socket: WebSocket, ip: IpAddr, conn_id: Uuid, state: AppState) {
    struct ConnGuard {
        metrics: Arc<Metrics>,
    }
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            self.metrics.active_connections.fetch_sub(1, Ordering::Relaxed);
        }
    }

    let _guard = ConnGuard {
        metrics: state.metrics.clone(),
    };

    loop {
        let next = tokio::time::timeout(state.config.idle_timeout, socket.next()).await;
        let msg = match next {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(_))) => break,
            Ok(None) => break,
            Err(_) => {
                // Idle timeout.
                let _ = socket.close().await;
                break;
            }
        };

        let Message::Binary(frame) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };

        if frame.len() > MAX_WS_FRAME_BYTES {
            response_jitter(&state.config).await;
            let _ = send_resp(
                &mut socket,
                ServerResponse::Error {
                    code: ErrorCode::Internal,
                },
            )
            .await;
            let _ = socket.close().await;
            break;
        }

        let req: ClientRequest = match bincode::deserialize(&frame) {
            Ok(r) => r,
            Err(_) => {
                response_jitter(&state.config).await;
                let _ = send_resp(&mut socket, privacy_error()).await;
                continue;
            }
        };

        state.metrics.requests_total.fetch_add(1, Ordering::Relaxed);

        let resp = match handle_request(&state, ip, req).await {
            Ok(r) => r,
            Err(e) => {
                error!(%conn_id, err = %e, "request failed");
                state.metrics.db_errors_total.fetch_add(1, Ordering::Relaxed);
                privacy_error()
            }
        };

        response_jitter(&state.config).await;
        if send_resp(&mut socket, resp).await.is_err() {
            break;
        }
    }

    info!(%conn_id, "ws disconnected");
}

async fn handle_request(
    state: &AppState,
    ip: IpAddr,
    req: ClientRequest,
) -> anyhow::Result<ServerResponse> {
    match req {
        ClientRequest::PutMessage {
            mailbox_id,
            message_id,
            ciphertext,
            ttl,
        } => {
            if ciphertext.len() > MAX_MESSAGE_BLOB_BYTES {
                return Ok(privacy_error());
            }
            if ttl <= 0 {
                return Ok(privacy_error());
            }
            if ttl > MAX_MESSAGE_TTL_SECS {
                return Ok(privacy_error());
            }

            if !rate_limit_put_message(&state.rate_limits, ip) {
                state
                    .metrics
                    .rate_limited_total
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(privacy_error());
            }

            let now = now_epoch_seconds();
            let expires_at = BsonDateTime::from_millis((now + ttl) * 1000);

            let filter = doc! {
                "mailbox_id": bson_bin_32(mailbox_id),
                "message_id": bson_bin_16(message_id),
            };

            let update = doc! {
                "$set": {
                    "ciphertext": Binary { subtype: BinarySubtype::Generic, bytes: ciphertext },
                    "timestamp": now,
                    "ttl": ttl,
                    "expires_at": expires_at,
                }
            };

            state
                .mailbox_messages
                .update_one(filter, update, UpdateOptions::builder().upsert(true).build())
                .await?;

            Ok(ServerResponse::Ok)
        }

        ClientRequest::FetchMessages { mailbox_id, limit } => {
            // Limit concurrent fetches per IP to reduce scraping/DoS.
            let sem = state
                .fetch_limits
                .entry(ip)
                .or_insert_with(|| Arc::new(Semaphore::new(state.config.max_concurrent_fetch_per_ip)))
                .clone();
            let _permit = match sem.try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    state
                        .metrics
                        .rate_limited_total
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(privacy_error());
                }
            };

            let limit = if limit == 0 { DEFAULT_FETCH_LIMIT } else { limit };
            let limit = limit.min(MAX_FETCH_LIMIT);

            let filter = doc! { "mailbox_id": bson_bin_32(mailbox_id) };
            let options = FindOptions::builder()
                .sort(doc! { "timestamp": 1 })
                .limit(i64::from(limit))
                .build();

            let mut cursor = state.mailbox_messages.find(filter, options).await?;
            let mut messages = Vec::new();
            while let Some(doc) = cursor.next().await {
                let doc = doc?;
                if let Some(out) = doc_to_fetched_message(doc) {
                    messages.push(out);
                }
            }

            // Track mailbox fetch activity without storing any identity.
            let now = now_epoch_seconds();
            let filter = doc! { "mailbox_id": bson_bin_32(mailbox_id) };
            let update = doc! { "$set": { "last_fetch": now } };
            let _ = state
                .mailbox_activity
                .update_one(filter, update, UpdateOptions::builder().upsert(true).build())
                .await;

            Ok(ServerResponse::Messages { messages })
        }

        ClientRequest::DeleteMessages {
            mailbox_id,
            message_ids,
        } => {
            if message_ids.is_empty() {
                return Ok(ServerResponse::Ok);
            }

            let ids_bson: Vec<Bson> = message_ids
                .into_iter()
                .map(|id| Bson::Binary(bson_bin_16(id)))
                .collect();

            let filter = doc! {
                "mailbox_id": bson_bin_32(mailbox_id),
                "message_id": { "$in": ids_bson },
            };
            let _ = state.mailbox_messages.delete_many(filter, None).await?;
            Ok(ServerResponse::Ok)
        }

        ClientRequest::UploadPrekey { bundle_id, payload } => {
            if payload.len() > MAX_PREKEY_BUNDLE_BYTES {
                return Ok(privacy_error());
            }

            if !rate_limit_upload_prekey(&state.rate_limits, ip) {
                state
                    .metrics
                    .rate_limited_total
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(privacy_error());
            }

            let uploaded_at = now_epoch_seconds();
            let expires_at = BsonDateTime::from_millis((uploaded_at + PREKEY_TTL_SECS) * 1000);
            let filter = doc! { "bundle_id": bson_bin_16(bundle_id) };
            let update = doc! {
                "$set": {
                    "public_payload": Binary { subtype: BinarySubtype::Generic, bytes: payload },
                    "uploaded_at": uploaded_at,
                    "expires_at": expires_at,
                }
            };

            state
                .prekeys
                .update_one(filter, update, UpdateOptions::builder().upsert(true).build())
                .await?;
            Ok(ServerResponse::Ok)
        }

        ClientRequest::FetchPrekey { bundle_id } => {
            let filter = doc! { "bundle_id": bson_bin_16(bundle_id) };
            let doc = state.prekeys.find_one(filter, None).await?;
            let Some(doc) = doc else {
                // Normalize existence: always return a payload (possibly empty).
                return Ok(ServerResponse::Prekey { payload: Vec::new() });
            };

            let payload = doc
                .get_binary_generic("public_payload")
                .map(|b| b.to_vec())
                .unwrap_or_default();

            Ok(ServerResponse::Prekey { payload })
        }
    }
}

fn privacy_error() -> ServerResponse {
    // Normalize errors: same variant, same code.
    ServerResponse::Error {
        code: ErrorCode::Internal,
    }
}

async fn response_jitter(config: &Config) {
    let min = config.jitter_min.as_millis() as u64;
    let max = config.jitter_max.as_millis() as u64;
    let ms = if max <= min {
        min
    } else {
        thread_rng().gen_range(min..=max)
    };
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn send_resp(socket: &mut WebSocket, resp: ServerResponse) -> Result<(), ()> {
    let bytes = bincode::serialize(&resp).map_err(|_| ())?;
    socket.send(Message::Binary(bytes)).await.map_err(|_| ())
}

fn bson_bin_16(v: [u8; 16]) -> Binary {
    Binary {
        subtype: BinarySubtype::Generic,
        bytes: v.to_vec(),
    }
}

fn bson_bin_32(v: [u8; 32]) -> Binary {
    Binary {
        subtype: BinarySubtype::Generic,
        bytes: v.to_vec(),
    }
}

fn doc_to_fetched_message(doc: Document) -> Option<FetchedMessage> {
    let message_id = doc
        .get_binary_generic("message_id")
        .ok()
        .and_then(|b| <[u8; 16]>::try_from(b.as_slice()).ok())?;
    let ciphertext = doc
        .get_binary_generic("ciphertext")
        .ok()
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let timestamp = doc.get_i64("timestamp").ok().unwrap_or_default();
    let ttl = doc.get_i64("ttl").ok().unwrap_or_default();
    Some(FetchedMessage {
        message_id,
        ciphertext,
        timestamp,
        ttl,
    })
}

fn now_epoch_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn spawn_garbage_collector(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let now = BsonDateTime::from_millis(now_epoch_seconds() * 1000);
            let filter = doc! { "expires_at": { "$lt": now } };
            match state.mailbox_messages.delete_many(filter, None).await {
                Ok(result) => {
                    // System-level metric only; do not log mailbox/message IDs.
                    if result.deleted_count > 0 {
                        info!(deleted = result.deleted_count, "gc deleted expired messages");
                    }
                }
                Err(e) => {
                    warn!(err = %e, "gc failed");
                }
            }
        }
    });
}

async fn ensure_indexes(
    mailbox_messages: &Collection<Document>,
    mailbox_activity: &Collection<Document>,
    prekeys: &Collection<Document>,
) -> anyhow::Result<()> {
    let mailbox_id_index = IndexModel::builder()
        .keys(doc! { "mailbox_id": 1 })
        .options(None)
        .build();

    let timestamp_index = IndexModel::builder()
        .keys(doc! { "timestamp": 1 })
        .options(None)
        .build();

    let compound_unique = IndexModel::builder()
        .keys(doc! { "mailbox_id": 1, "message_id": 1 })
        .options(
            IndexOptions::builder()
                .unique(true)
                .name("mailbox_message_unique".to_string())
                .build(),
        )
        .build();

    let expires_at_index = IndexModel::builder()
        .keys(doc! { "expires_at": 1 })
        .options(
            IndexOptions::builder()
                .name("expires_at_ttl".to_string())
                .expire_after(Some(Duration::from_secs(0)))
                .build(),
        )
        .build();

    mailbox_messages
        .create_indexes(
            vec![mailbox_id_index, timestamp_index, compound_unique, expires_at_index],
            None,
        )
        .await?;

    let mailbox_activity_unique = IndexModel::builder()
        .keys(doc! { "mailbox_id": 1 })
        .options(
            IndexOptions::builder()
                .unique(true)
                .name("mailbox_activity_unique".to_string())
                .build(),
        )
        .build();
    let mailbox_activity_last_fetch = IndexModel::builder()
        .keys(doc! { "last_fetch": 1 })
        .options(IndexOptions::builder().name("mailbox_activity_last_fetch".to_string()).build())
        .build();
    mailbox_activity
        .create_indexes(vec![mailbox_activity_unique, mailbox_activity_last_fetch], None)
        .await?;

    let bundle_unique = IndexModel::builder()
        .keys(doc! { "bundle_id": 1 })
        .options(
            IndexOptions::builder()
                .unique(true)
                .name("bundle_unique".to_string())
                .build(),
        )
        .build();

    let prekey_expires_at = IndexModel::builder()
        .keys(doc! { "expires_at": 1 })
        .options(
            IndexOptions::builder()
                .name("prekey_expires_at_ttl".to_string())
                .expire_after(Some(Duration::from_secs(0)))
                .build(),
        )
        .build();

    prekeys
        .create_indexes(vec![bundle_unique, prekey_expires_at], None)
        .await?;
    Ok(())
}

fn rate_limit_put_message(rate_limits: &IpRateLimiter, ip: IpAddr) -> bool {
    // 60 PutMessage per minute per IP.
    let now = now_epoch_seconds();
    let mut entry = rate_limits
        .put_message
        .entry(ip)
        .or_insert_with(|| TokenBucket::new_full(60, now));
    entry.allow(60, 60, now)
}

fn rate_limit_upload_prekey(rate_limits: &IpRateLimiter, ip: IpAddr) -> bool {
    // 10 UploadPrekey per minute per IP.
    let now = now_epoch_seconds();
    let mut entry = rate_limits
        .upload_prekey
        .entry(ip)
        .or_insert_with(|| TokenBucket::new_full(10, now));
    entry.allow(10, 10, now)
}
