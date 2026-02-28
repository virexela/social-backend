use axum::{
    Router,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use uuid::Uuid;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

type Tx = UnboundedSender<Message>;

#[derive(Clone)]
struct AppState {
    chat_rooms: Arc<DashMap<String, Arc<DashMap<String, Tx>>>>,
    invite_rooms: Arc<DashMap<String, Arc<DashMap<String, Tx>>>>,
    invite_room_limits: Arc<DashMap<String, usize>>,
    ws_rate_limit: usize,
    ws_window_secs: u64,
    http_rate_limit: usize,
    http_window_secs: u64,
    max_ws_text_len: usize,
    max_room_connections: usize,
    max_room_id_len: usize,
    // per-connection sliding-window timestamps for WebSocket message rate limiting
    ws_message_counters: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
    // per-IP HTTP request counters (sliding window)
    http_request_counters: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3001".to_string());
    let ws_rate_limit = read_usize_env("WS_RATE_LIMIT", 30);
    let ws_window_secs = read_u64_env("WS_WINDOW_SECS", 5);
    let http_rate_limit_per_window = read_usize_env("HTTP_RATE_LIMIT", 120);
    let http_window_secs = read_u64_env("HTTP_WINDOW_SECS", 60);
    let max_ws_text_len = read_usize_env("MAX_WS_TEXT_LEN", 1_048_576);
    let max_room_connections = read_usize_env("MAX_ROOM_CONNECTIONS", 200);
    let max_room_id_len = read_usize_env("MAX_ROOM_ID_LEN", 128);

    // initial state (include ws counter map)
    let state = AppState {
        chat_rooms: Arc::new(DashMap::new()),
        invite_rooms: Arc::new(DashMap::new()),
        invite_room_limits: Arc::new(DashMap::new()),
        ws_rate_limit,
        ws_window_secs,
        http_rate_limit: http_rate_limit_per_window,
        http_window_secs,
        max_ws_text_len,
        max_room_connections,
        max_room_id_len,
        ws_message_counters: Arc::new(Mutex::new(HashMap::new())),
        http_request_counters: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/:room", get(ws_handler))
        .route("/invite-ws/:room", get(invite_ws_handler))
        // apply a small per-IP HTTP rate limiter middleware
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            http_rate_limit,
        ))
        .with_state(state);

    println!("listening on {}", bind_addr);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    // required by tower-governor so peer SocketAddr can be extracted
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

fn is_valid_room(room: &str, max_len: usize) -> bool {
    if room.is_empty() || room.len() > max_len {
        return false;
    }
    room.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

fn read_usize_env(name: &str, fallback: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}

fn read_u64_env(name: &str, fallback: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}

async fn ws_handler(
    Path(room): Path<String>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !is_valid_room(&room, state.max_room_id_len) {
        return (axum::http::StatusCode::BAD_REQUEST, "invalid_room").into_response();
    }
    ws.on_upgrade(move |s| chat_socket(s, room, state))
        .into_response()
}

async fn chat_socket(socket: WebSocket, room: String, state: AppState) {
    println!("[ws] joined room {}", room);
    let (tx, mut rx) = unbounded_channel::<Message>();
    let room_map = state
        .chat_rooms
        .entry(room.clone())
        .or_insert_with(|| Arc::new(DashMap::new()))
        .clone();
    if room_map.len() >= state.max_room_connections {
        return;
    }
    let conn_id = Uuid::new_v4().to_string();
    room_map.insert(conn_id.clone(), tx);

    let (mut ws_tx, mut ws_rx) = socket.split();
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        if let Message::Text(t) = msg {
            if t.len() > state.max_ws_text_len {
                if let Some(me) = room_map.get(&conn_id) {
                    let _ = me.value().send(Message::Text(
                        json!({ "error": "payload_too_large" }).to_string(),
                    ));
                }
                continue;
            }
            // per-connection rate limit
            if !allow_ws_message(&state, &conn_id).await {
                if let Some(me) = room_map.get(&conn_id) {
                    let _ = me.value().send(Message::Text(
                        json!({ "error": "rate_limited" }).to_string(),
                    ));
                }
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                if let Some(ciphertext) = v.get("ciphertext").and_then(|x| x.as_str()) {
                    let out = json!({ "from": conn_id, "ciphertext": ciphertext });
                    for r in room_map.iter().filter(|r| r.key() != &conn_id) {
                        let _ = r.value().send(Message::Text(out.to_string()));
                    }
                }
            }
        } else if matches!(msg, Message::Close(_)) {
            break;
        }
    }

    // cleanup rate-limit state for this connection
    {
        let mut counters = state.ws_message_counters.lock().await;
        counters.remove(&conn_id);
    }

    room_map.remove(&conn_id);
    if room_map.is_empty() {
        state.chat_rooms.remove(&room);
    }
    send_task.abort();
    println!("[ws] connection {} left room {}", conn_id, room);
}

#[derive(Deserialize, Default)]
struct InviteWsQuery {
    limit: Option<usize>,
    creator: Option<String>,
}

async fn invite_ws_handler(
    Path(room): Path<String>,
    Query(query): Query<InviteWsQuery>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !is_valid_room(&room, state.max_room_id_len) {
        return (axum::http::StatusCode::BAD_REQUEST, "invalid_room").into_response();
    }
    ws.on_upgrade(move |s| invite_socket(s, room, state, query))
        .into_response()
}

async fn allow_ws_message(state: &AppState, conn: &str) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(state.ws_window_secs);

    let mut map = state.ws_message_counters.lock().await;
    let dq = map.entry(conn.to_string()).or_insert_with(VecDeque::new);

    // drop old timestamps
    while dq
        .front()
        .map(|t| now.duration_since(*t) > window)
        .unwrap_or(false)
    {
        dq.pop_front();
    }

    if dq.len() >= state.ws_rate_limit {
        false
    } else {
        dq.push_back(now);
        true
    }
}

async fn allow_http_request(state: &AppState, key: &str) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(state.http_window_secs);

    let mut map = state.http_request_counters.lock().await;
    let dq = map.entry(key.to_string()).or_insert_with(VecDeque::new);

    while dq
        .front()
        .map(|t| now.duration_since(*t) > window)
        .unwrap_or(false)
    {
        dq.pop_front();
    }

    if dq.len() >= state.http_rate_limit {
        false
    } else {
        dq.push_back(now);
        true
    }
}

async fn http_rate_limit(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    // peer SocketAddr is added by into_make_service_with_connect_info when the server is started
    let peer = req
        .extensions()
        .get::<SocketAddr>()
        .map(|s| s.ip().to_string())
        .unwrap_or_else(|| "unknown".into());

    if !allow_http_request(&state, &peer).await {
        let body = serde_json::json!({ "error": "too_many_requests" }).to_string();
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
            body,
        )
            .into_response();
    }

    next.run(req).await
}

const DEFAULT_INVITE_ROOM_LIMIT: usize = 2;
const MAX_INVITE_ROOM_LIMIT: usize = 50;

fn normalize_invite_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_INVITE_ROOM_LIMIT)
        .clamp(DEFAULT_INVITE_ROOM_LIMIT, MAX_INVITE_ROOM_LIMIT)
}

fn is_creator_flag(raw: Option<&str>) -> bool {
    matches!(raw, Some("1") | Some("true") | Some("yes"))
}

async fn invite_socket(mut socket: WebSocket, room: String, state: AppState, query: InviteWsQuery) {
    // anonymous connection into invite room
    let (tx, mut rx) = unbounded_channel::<Message>();
    let room_map = state
        .invite_rooms
        .entry(room.clone())
        .or_insert_with(|| Arc::new(DashMap::new()))
        .clone();
    if room_map.len() >= state.max_room_connections {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    let requested_limit = normalize_invite_limit(query.limit);
    let is_creator = is_creator_flag(query.creator.as_deref());
    if is_creator || !state.invite_room_limits.contains_key(&room) {
        state
            .invite_room_limits
            .insert(room.clone(), requested_limit);
    }
    let room_limit = state
        .invite_room_limits
        .get(&room)
        .map(|v| *v.value())
        .unwrap_or(DEFAULT_INVITE_ROOM_LIMIT);

    if room_map.len() >= room_limit {
        let _ = socket
            .send(Message::Text(
                json!({ "type": "error", "error": "invite_limit_reached" }).to_string(),
            ))
            .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    let conn_id = Uuid::new_v4().to_string();
    room_map.insert(conn_id.clone(), tx);

    let notice = Message::Text(json!({ "type": "invite_accepted", "by": conn_id }).to_string());
    for r in room_map.iter() {
        if r.key() != &conn_id {
            let _ = r.value().send(notice.clone());
        }
    }

    let (mut ws_tx, mut ws_rx) = socket.split();
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = ws_rx.next().await {
        if matches!(msg, Message::Close(_)) {
            break;
        }
    }

    room_map.remove(&conn_id);
    if room_map.is_empty() {
        state.invite_rooms.remove(&room);
        state.invite_room_limits.remove(&room);
    }
    send_task.abort();
}
