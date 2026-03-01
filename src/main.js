import express from 'express';
import { WebSocketServer } from 'ws';
import { createServer } from 'http';
import { v4 as uuidv4 } from 'uuid';

// Utility functions
function readEnv(name, fallback) {
  const value = process.env[name];
  if (value === undefined) return fallback;
  const parsed = parseInt(value, 10);
  return !isNaN(parsed) && parsed > 0 ? parsed : fallback;
}

function isValidRoom(room, maxLen) {
  if (!room || room.length === 0 || room.length > maxLen) {
    return false;
  }
  return /^[a-zA-Z0-9\-_:]+$/.test(room);
}

// Rate limiting functions
async function allowWsMessage(state, connId) {
  const now = Date.now();
  const window = state.wsWindowMs;

  let dq = state.wsMessageCounters.get(connId);
  if (!dq) {
    dq = [];
    state.wsMessageCounters.set(connId, dq);
  }

  // Remove old timestamps outside the window
  while (dq.length > 0 && now - dq[0] > window) {
    dq.shift();
  }

  if (dq.length >= state.wsRateLimit) {
    return false;
  }

  dq.push(now);
  return true;
}

async function allowHttpRequest(state, key) {
  const now = Date.now();
  const window = state.httpWindowMs;

  let dq = state.httpRequestCounters.get(key);
  if (!dq) {
    dq = [];
    state.httpRequestCounters.set(key, dq);
  }

  while (dq.length > 0 && now - dq[0] > window) {
    dq.shift();
  }

  if (dq.length >= state.httpRateLimit) {
    return false;
  }

  dq.push(now);
  return true;
}

function getClientIp(req) {
  // Prefer x-forwarded-for from proxies, then x-real-ip, then socket address
  const xForwardedFor = req.headers['x-forwarded-for'];
  if (xForwardedFor) {
    const ip = xForwardedFor.split(',')[0].trim();
    if (ip) return ip;
  }

  const xRealIp = req.headers['x-real-ip'];
  if (xRealIp) {
    const ip = xRealIp.trim();
    if (ip) return ip;
  }

  return req.socket.remoteAddress || 'unknown';
}

async function handleHttpRateLimit(req, res, next) {
  const clientIp = getClientIp(req);

  if (!(await allowHttpRequest(req.app.locals.state, clientIp))) {
    res.status(429).json({ error: 'too_many_requests' });
    return;
  }

  next();
}

// Main server setup
const bindAddr = process.env.BIND_ADDR || '0.0.0.0:8080';
const [host, port] = bindAddr.split(':');

const wsRateLimit = readEnv('WS_RATE_LIMIT', 30);
const wsWindowSecs = readEnv('WS_WINDOW_SECS', 5);
const httpRateLimit = readEnv('HTTP_RATE_LIMIT', 120);
const httpWindowSecs = readEnv('HTTP_WINDOW_SECS', 60);
const maxWsTextLen = readEnv('MAX_WS_TEXT_LEN', 1_048_576);
const maxRoomConnections = readEnv('MAX_ROOM_CONNECTIONS', 200);
const maxRoomIdLen = readEnv('MAX_ROOM_ID_LEN', 128);

const state = {
  chatRooms: new Map(),
  inviteRooms: new Map(),
  inviteRoomLimits: new Map(),
  wsRateLimit,
  wsWindowMs: wsWindowSecs * 1000,
  httpRateLimit,
  httpWindowMs: httpWindowSecs * 1000,
  maxWsTextLen,
  maxRoomConnections,
  maxRoomIdLen,
  wsMessageCounters: new Map(),
  httpRequestCounters: new Map(),
};

const app = express();
const server = createServer(app);

app.locals.state = state;

// HTTP middleware for rate limiting
app.use(handleHttpRateLimit);

// Health check endpoint
app.get('/healthz', (req, res) => {
  res.send('ok');
});

server.on('upgrade', (req, socket, head) => {
  const url = req.url;

  if (url.startsWith('/ws/')) {
    const room = url.slice(4);
    wss.handleUpgrade(req, socket, head, (ws) => {
      chatHandler(ws, room, state);
    });
  } else if (url.startsWith('/invite-ws/')) {
    const match = url.match(/^\/invite-ws\/([^?]*)/);
    if (match) {
      const room = match[1];
      const query = new URL(url, `http://${req.headers.host}`).searchParams;
      const inviteQuery = {
        limit: query.get('limit') ? parseInt(query.get('limit'), 10) : undefined,
        creator: query.get('creator'),
      };
      wss.handleUpgrade(req, socket, head, (ws) => {
        inviteHandler(ws, room, state, inviteQuery);
      });
    }
  } else {
    socket.destroy();
  }
});

const wss = new WebSocketServer({ noServer: true });

function isValidJson(str) {
  try {
    JSON.parse(str);
    return true;
  } catch {
    return false;
  }
}

async function chatHandler(ws, room, state) {
  if (!isValidRoom(room, state.maxRoomIdLen)) {
    ws.close(1008, 'invalid_room');
    return;
  }

  console.log(`[ws] joined room ${room}`);

  const connId = uuidv4();
  const roomMap = state.chatRooms.get(room) || new Map();

  if (roomMap.size >= state.maxRoomConnections) {
    ws.close(1008, 'room_full');
    return;
  }

  state.chatRooms.set(room, roomMap);
  roomMap.set(connId, ws);

  ws.on('message', async (data) => {
    try {
      const text = data.toString('utf-8');

      if (text.length > state.maxWsTextLen) {
        ws.send(JSON.stringify({ error: 'payload_too_large' }));
        return;
      }

      // Per-connection rate limit
      if (!(await allowWsMessage(state, connId))) {
        ws.send(JSON.stringify({ error: 'rate_limited' }));
        return;
      }

      if (!isValidJson(text)) {
        return;
      }

      const obj = JSON.parse(text);

      if (obj.ciphertext && typeof obj.ciphertext === 'string') {
        const outMsg = { from: connId, ciphertext: obj.ciphertext };
        for (const [key, client] of roomMap.entries()) {
          if (key !== connId && client.readyState === 1) {
            // 1 = OPEN
            client.send(JSON.stringify(outMsg));
          }
        }
      }
    } catch (err) {
      console.error('Error handling message:', err);
    }
  });

  ws.on('close', () => {
    roomMap.delete(connId);
    if (roomMap.size === 0) {
      state.chatRooms.delete(room);
    }
    state.wsMessageCounters.delete(connId);
    console.log(`[ws] connection ${connId} left room ${room}`);
  });

  ws.on('error', (err) => {
    console.error(`[ws] error:`, err);
  });
}

const DEFAULT_INVITE_ROOM_LIMIT = 2;
const MAX_INVITE_ROOM_LIMIT = 50;

function normalizeInviteLimit(limit) {
  if (limit === undefined) return DEFAULT_INVITE_ROOM_LIMIT;
  return Math.max(
    DEFAULT_INVITE_ROOM_LIMIT,
    Math.min(limit, MAX_INVITE_ROOM_LIMIT)
  );
}

function isCreatorFlag(raw) {
  return raw === '1' || raw === 'true' || raw === 'yes';
}

async function inviteHandler(ws, room, state, query) {
  if (!isValidRoom(room, state.maxRoomIdLen)) {
    ws.close(1008, 'invalid_room');
    return;
  }

  const roomMap = state.inviteRooms.get(room) || new Map();

  if (roomMap.size >= state.maxRoomConnections) {
    ws.close(1008, 'room_full');
    return;
  }

  state.inviteRooms.set(room, roomMap);

  const requestedLimit = normalizeInviteLimit(query.limit);
  const isCreator = isCreatorFlag(query.creator);

  if (isCreator || !state.inviteRoomLimits.has(room)) {
    state.inviteRoomLimits.set(room, requestedLimit);
  }

  const roomLimit =
    state.inviteRoomLimits.get(room) || DEFAULT_INVITE_ROOM_LIMIT;

  if (roomMap.size >= roomLimit) {
    ws.send(
      JSON.stringify({ type: 'error', error: 'invite_limit_reached' })
    );
    ws.close(1008, 'invite_limit_reached');
    return;
  }

  const connId = uuidv4();
  roomMap.set(connId, ws);

  const notice = { type: 'invite_accepted', by: connId };
  for (const [key, client] of roomMap.entries()) {
    if (key !== connId && client.readyState === 1) {
      client.send(JSON.stringify(notice));
    }
  }

  ws.on('message', () => {
    // Messages are not processed in invite rooms
  });

  ws.on('close', () => {
    roomMap.delete(connId);
    if (roomMap.size === 0) {
      state.inviteRooms.delete(room);
      state.inviteRoomLimits.delete(room);
    }
    console.log(`[invite-ws] connection ${connId} left room ${room}`);
  });

  ws.on('error', (err) => {
    console.error(`[invite-ws] error:`, err);
  });
}

server.listen(parseInt(port), host, () => {
  console.log(`listening on ${host}:${port}`);
});
