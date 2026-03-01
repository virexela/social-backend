# Social Backend - Node.js WebSocket Server

This is a Node.js implementation of the WebSocket backend for the social group application, converted from Rust/Axum.

## Installation

```bash
npm install
# or
pnpm install
```

## Running

```bash
# Development with auto-reload
npm run dev

# Production
npm start
```

## Configuration

All configuration is done via environment variables:

- `BIND_ADDR` (default: `0.0.0.0:8080`) - Server address and port
- `WS_RATE_LIMIT` (default: `30`) - Max WebSocket messages per window
- `WS_WINDOW_SECS` (default: `5`) - WebSocket rate limit window in seconds
- `HTTP_RATE_LIMIT` (default: `120`) - Max HTTP requests per window per IP
- `HTTP_WINDOW_SECS` (default: `60`) - HTTP rate limit window in seconds
- `MAX_WS_TEXT_LEN` (default: `1048576`) - Max WebSocket message size in bytes
- `MAX_ROOM_CONNECTIONS` (default: `200`) - Max users per room
- `MAX_ROOM_ID_LEN` (default: `128`) - Max room ID length

## API Endpoints

### HTTP

- `GET /healthz` - Health check endpoint (returns "ok")

### WebSocket

#### `/ws/:room`
Chat room WebSocket endpoint. Supports:
- Per-connection sliding window rate limiting
- Message relaying between room members
- Automatic cleanup on disconnect
- Payload size validation

Messages must be JSON with a `ciphertext` field:
```json
{
  "ciphertext": "..."
}
```

Relayed messages include:
```json
{
  "from": "connection-id",
  "ciphertext": "..."
}
```

#### `/invite-ws/:room`
Invite room WebSocket endpoint. Supports:
- Connection limits per room
- Anonymous connections
- Creator-defined room capacity

Query parameters:
- `limit` (optional): Room capacity (clamped between 2-50, default 2)
- `creator` (optional): Set to "1", "true", or "yes" to mark as creator

Messages sent on successful join:
```json
{
  "type": "invite_accepted",
  "by": "connection-id"
}
```

Error message:
```json
{
  "type": "error",
  "error": "invite_limit_reached"
}
```

## Features

### Rate Limiting

- **WebSocket Rate Limiting**: Per-connection sliding window using timestamps
- **HTTP Rate Limiting**: Per-IP sliding window middleware
- Configurable limits and windows via environment variables

### Room Management

- Automatic room creation on first join
- Automatic room cleanup when empty
- Connection limit enforcement
- Per-room invite capacity limits

### Validation

- Room ID validation (alphanumeric, dashes, underscores, colons only)
- Message payload size validation
- JSON validation for incoming messages
- IP extraction from proxies (X-Forwarded-For, X-Real-IP headers)

## Differences from Rust Implementation

The Node.js implementation maintains functional parity with the Rust version:

- Uses `ws` library for WebSocket support
- Uses JavaScript `Map` for state storage instead of `DashMap`
- Rate limiting uses `Map<string, number[]>` for sliding window timestamps
- Uses Express.js for HTTP routing
- Message validation uses native JSON parsing
- Client IP extraction handles proxy headers identically
- All configuration values and defaults match the Rust version

## Architecture

```
server (HTTP + upgrade handling)
├── HTTP middleware (rate limiting)
├── /healthz (health check)
└── WebSocket upgrade handler
    ├── /ws/:room (chat rooms)
    └── /invite-ws/:room (invite rooms)
```

## Error Handling

- `invalid_room`: Room ID fails validation
- `room_full`: Room has reached max connections
- `invite_limit_reached`: Invite room has reached capacity
- `payload_too_large`: Message exceeds max size
- `rate_limited`: Rate limit exceeded
- `too_many_requests`: HTTP rate limit exceeded (429 status)
