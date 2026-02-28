# Social Backend (Relay)

Rust websocket relay for chat and invite channels.

## Production Setup

1. Copy env template:
```bash
cp .env.example .env
```
2. Build and run:
```bash
cargo build --release
./target/release/social-backend
```

## Environment Variables

- `BIND_ADDR` (default `0.0.0.0:3000`)
- `RUST_LOG` (default `info`)
- `WS_RATE_LIMIT` / `WS_WINDOW_SECS`
- `HTTP_RATE_LIMIT` / `HTTP_WINDOW_SECS`
- `MAX_WS_TEXT_LEN`
- `MAX_ROOM_CONNECTIONS`
- `MAX_ROOM_ID_LEN`

## Health Check

- `GET /healthz` returns `ok`.

## Security and Hardening Baseline

- Per-IP HTTP rate limiting middleware.
- Per-connection websocket message rate limiting.
- Room ID validation.
- Max websocket payload length guard.
- Max room connection cap.
- Invite room participant limit enforcement.

## Container

```bash
docker build -t social-backend .
docker run --rm -p 3000:3000 --env-file .env social-backend
```
