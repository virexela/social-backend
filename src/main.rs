use axum::{extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State}, response::IntoResponse, routing::get, Router};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

type Tx = UnboundedSender<Message>;

#[derive(Clone)]
struct AppState {
    peers: Arc<DashMap<String, Tx>>,
    invite_rooms: Arc<DashMap<String, Arc<DashMap<String, Tx>>>>,
    // per-user sliding-window timestamps for WebSocket message rate limiting
    ws_message_counters: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
    // per-IP HTTP request counters (sliding window)
    http_request_counters: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
}

#[derive(Deserialize)]
struct ChatRequest { to: String, text: String }

#[derive(Serialize)]
struct OutgoingMessage<'a> { from: &'a str, text: &'a str }

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // initial state (include ws counter map)
    let state = AppState {
        peers: Arc::new(DashMap::new()),
        invite_rooms: Arc::new(DashMap::new()),
        ws_message_counters: Arc::new(Mutex::new(HashMap::new())),
        http_request_counters: Arc::new(Mutex::new(HashMap::new())),
    };


    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/:user", get(ws_handler))
        .route("/invite-ws/:room/:user", get(invite_ws_handler))
        // apply a small per-IP HTTP rate limiter middleware
        .layer(axum::middleware::from_fn_with_state(state.clone(), http_rate_limit))
        .with_state(state);

    println!("listening on 0.0.0.0:3001");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
    // required by tower-governor so peer SocketAddr can be extracted
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
}

async fn ws_handler(Path(user): Path<String>, State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse { ws.on_upgrade(move |s| chat_socket(s, user, state)) }

async fn chat_socket(socket: WebSocket, user: String, state: AppState) {
    println!("[ws] {} connected", user);
    let (tx, mut rx) = unbounded_channel::<Message>();
    state.peers.insert(user.clone(), tx);
    let (mut ws_tx, mut ws_rx) = socket.split();

    let send_task = tokio::spawn(async move { while let Some(msg) = rx.recv().await { if ws_tx.send(msg).await.is_err() { break } } });

    while let Some(Ok(msg)) = ws_rx.next().await {
        if let Message::Text(t) = msg {
            // per-user WebSocket message rate limit (sliding window of 1s)
            if !allow_ws_message(&state, &user).await {
                if let Some(me) = state.peers.get(&user) {
                    let _ = me.value().send(Message::Text(json!({ "error": "rate_limited" }).to_string()));
                }
                continue;
            }

            match serde_json::from_str::<ChatRequest>(&t) {
                Ok(req) => if let Some(dest) = state.peers.get(&req.to) {
                    let _ = dest.value().send(Message::Text(serde_json::to_string(&OutgoingMessage { from: &user, text: &req.text }).unwrap()));
                } else if let Some(me) = state.peers.get(&user) {
                    let _ = me.value().send(Message::Text(json!({ "error": "recipient not connected" }).to_string()));
                },
                Err(_) => if let Some(me) = state.peers.get(&user) { let _ = me.value().send(Message::Text(json!({ "error": "invalid message" }).to_string())); },
            }
        } else if matches!(msg, Message::Close(_)) { break }
    }

    // cleanup rate-limit state for this user
    {
        let mut counters = state.ws_message_counters.lock().await;
        counters.remove(&user);
    }

    state.peers.remove(&user);
    send_task.abort();
    println!("[ws] {} disconnected", user);
}

async fn invite_ws_handler(Path((room, user)): Path<(String, String)>, State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse { ws.on_upgrade(move |s| invite_socket(s, room, user, state)) }

// simple sliding-window limiter for WebSocket messages (per-user)
const WS_RATE_LIMIT: usize = 5; // messages
const WS_WINDOW_SECS: u64 = 1; // window in seconds

async fn allow_ws_message(state: &AppState, user: &str) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(WS_WINDOW_SECS);

    let mut map = state.ws_message_counters.lock().await;
    let dq = map.entry(user.to_string()).or_insert_with(VecDeque::new);

    // drop old timestamps
    while dq.front().map(|t| now.duration_since(*t) > window).unwrap_or(false) {
        dq.pop_front();
    }

    if dq.len() >= WS_RATE_LIMIT {
        false
    } else {
        dq.push_back(now);
        true
    }
}

// simple per-IP HTTP sliding-window limiter
const HTTP_RATE_LIMIT: usize = 5; // requests
const HTTP_WINDOW_SECS: u64 = 1; // seconds

async fn allow_http_request(state: &AppState, key: &str) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(HTTP_WINDOW_SECS);

    let mut map = state.http_request_counters.lock().await;
    let dq = map.entry(key.to_string()).or_insert_with(VecDeque::new);

    while dq.front().map(|t| now.duration_since(*t) > window).unwrap_or(false) {
        dq.pop_front();
    }

    if dq.len() >= HTTP_RATE_LIMIT {
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
        return (axum::http::StatusCode::TOO_MANY_REQUESTS, [("content-type", "application/json")], body).into_response();
    }

    next.run(req).await
}

async fn invite_socket(socket: WebSocket, room: String, user: String, state: AppState) {
    println!("[invite-ws] {} -> {}", user, room);
    let (tx, mut rx) = unbounded_channel::<Message>();
    let room_map = state.invite_rooms.entry(room.clone()).or_insert_with(|| Arc::new(DashMap::new())).clone();
    room_map.insert(user.clone(), tx);

    let notice = Message::Text(json!({ "type": "invite_accepted", "by": user }).to_string());
    for r in room_map.iter() { if r.key() != &user { let _ = r.value().send(notice.clone()); } }

    let (mut ws_tx, mut ws_rx) = socket.split();
    let send_task = tokio::spawn(async move { while let Some(msg) = rx.recv().await { if ws_tx.send(msg).await.is_err() { break } } });
    while let Some(Ok(msg)) = ws_rx.next().await { if matches!(msg, Message::Close(_)) { break } }

    room_map.remove(&user);
    if room_map.is_empty() { state.invite_rooms.remove(&room); }
    send_task.abort();
    println!("[invite-ws] {} left {}", user, room);
}

