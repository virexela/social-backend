use axum::{extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State}, response::IntoResponse, routing::get, Router};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

type Tx = UnboundedSender<Message>;

#[derive(Clone)]
struct AppState { peers: Arc<DashMap<String, Tx>>, invite_rooms: Arc<DashMap<String, Arc<DashMap<String, Tx>>>> }

#[derive(Deserialize)]
struct ChatRequest { to: String, text: String }

#[derive(Serialize)]
struct OutgoingMessage<'a> { from: &'a str, text: &'a str }

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let state = AppState { peers: Arc::new(DashMap::new()), invite_rooms: Arc::new(DashMap::new()) };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/:user", get(ws_handler))
        .route("/invite-ws/:room/:user", get(invite_ws_handler))
        .with_state(state);

    println!("listening on 0.0.0.0:3001");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
    axum::serve(listener, app).await.unwrap();
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

    state.peers.remove(&user);
    send_task.abort();
    println!("[ws] {} disconnected", user);
}

async fn invite_ws_handler(Path((room, user)): Path<(String, String)>, State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse { ws.on_upgrade(move |s| invite_socket(s, room, user, state)) }

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

