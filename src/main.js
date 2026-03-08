import express from 'express';
import { WebSocketServer } from 'ws';
import { createServer } from 'http';
import { v4 as uuidv4 } from 'uuid';
import {
  DEFAULT_INVITE_ROOM_LIMIT,
  isCreatorFlag,
  isValidRoom,
  normalizeInviteLimit,
  parseBindAddr,
  readEnv,
  validateServerConfig,
  verifyWsJoinToken,
} from './serverUtils.js';

const RATE_LIMIT_REDIS_URL = process.env.RATE_LIMIT_REDIS_REST_URL?.trim();
const RATE_LIMIT_REDIS_TOKEN = process.env.RATE_LIMIT_REDIS_REST_TOKEN?.trim();
const WS_AUTH_SECRET = process.env.WS_AUTH_SECRET?.trim() ?? '';
const WS_AUTH_ENFORCE = !!WS_AUTH_SECRET && (process.env.WS_AUTH_ENFORCE === '1' || process.env.NODE_ENV === 'production');
validateServerConfig();

async function getDistributedRateLimitCount(key, ttlSeconds) {
  if (!RATE_LIMIT_REDIS_URL || !RATE_LIMIT_REDIS_TOKEN) return null;
  const encodedKey = encodeURIComponent(`rl:backend:${key}`);
  try {
    const incrRes = await fetch(`${RATE_LIMIT_REDIS_URL}/incr/${encodedKey}`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${RATE_LIMIT_REDIS_TOKEN}` },
    });
    if (!incrRes.ok) return null;

    const incrJson = await incrRes.json();
    const count = Number(incrJson?.result ?? 0);
    if (count === 1) {
      await fetch(`${RATE_LIMIT_REDIS_URL}/expire/${encodedKey}/${Math.ceil(ttlSeconds)}`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${RATE_LIMIT_REDIS_TOKEN}` },
      });
    }
    return count;
  } catch {
    return null;
  }
}

// Rate limiting functions
async function allowWsMessage(state, connId) {
  const distributed = await getDistributedRateLimitCount(
    `ws:${connId}`,
    state.wsWindowMs / 1000
  );
  if (distributed !== null) {
    return distributed <= state.wsRateLimit;
  }

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
  const distributed = await getDistributedRateLimitCount(
    `http:${key}`,
    state.httpWindowMs / 1000
  );
  if (distributed !== null) {
    return distributed <= state.httpRateLimit;
  }

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

// ✅ List of trusted proxy IP addresses/ranges
// Only requests from these proxies can have their X-Forwarded-For header trusted
const TRUSTED_PROXIES = [
  '127.0.0.1',           // Localhost
  '::1',                 // IPv6 localhost
  /^10\./,               // Private IP range 10.0.0.0/8
  /^172\.(1[6-9]|2\d|3[01])\./,  // Private IP range 172.16.0.0/12
  /^192\.168\./,         // Private IP range 192.168.0.0/16
  '::ffff:127.0.0.1',    // IPv6-mapped IPv4 localhost
];

function isTrustedProxy(ip) {
  if (!ip) return false;
  return TRUSTED_PROXIES.some(trusted => {
    if (typeof trusted === 'string') {
      return ip === trusted;
    }
    return trusted.test(ip);
  });
}

function getClientIp(req) {
  const directIp = req.socket.remoteAddress || 'unknown';

  // Only trust X-Forwarded-For if request came from a trusted proxy
  if (!isTrustedProxy(directIp)) {
    return directIp;
  }

  // Request came from trusted proxy - now safe to read X-Forwarded-For
  const xForwardedFor = req.headers['x-forwarded-for'];
  if (xForwardedFor) {
    // X-Forwarded-For can contain multiple IPs: client, proxy1, proxy2, ...
    // The first IP in the list is the client's original IP
    const ips = xForwardedFor.split(',').map(ip => ip.trim()).filter(Boolean);
    if (ips.length > 0) return ips[0];
  }

  // Fallback to x-real-ip if X-Forwarded-For not available
  const xRealIp = req.headers['x-real-ip'];
  if (xRealIp) {
    const ip = xRealIp.trim();
    if (ip) return ip;
  }

  return directIp;
}

async function handleHttpRateLimit(req, res, next) {
  const clientIp = getClientIp(req);

  if (!(await allowHttpRequest(req.app.locals.state, clientIp))) {
    // ✅ NEW: Add rate limit response headers
    res.set('RateLimit-Limit', req.app.locals.state.httpRateLimit.toString());
    res.set('RateLimit-Remaining', '0');
    res.set('RateLimit-Reset', (Date.now() + req.app.locals.state.httpWindowMs).toString());
    res.status(429).json({ error: 'too_many_requests' });
    return;
  }

  next();
}

// Main server setup
const { host, port } = parseBindAddr(process.env.BIND_ADDR, '0.0.0.0:8080');

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
  inviteRoomCreators: new Map(),
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
  if (req.query?.deep === '1') {
    try {
      validateServerConfig();
      res.status(200).json({ ok: true, service: 'social-backend', checks: { securityConfig: 'ok' } });
    } catch (err) {
      res.status(503).json({
        ok: false,
        service: 'social-backend',
        error: err instanceof Error ? err.message : String(err),
      });
    }
    return;
  }
  res.send('ok');
});

function rejectUpgrade(req, socket, statusCode, statusText, body) {
  if (!socket.writable) {
    socket.destroy();
    return;
  }

  const payload = `${body}\n`;
  socket.write(
    `HTTP/1.1 ${statusCode} ${statusText}\r\n` +
    'Content-Type: text/plain; charset=utf-8\r\n' +
    `Content-Length: ${Buffer.byteLength(payload)}\r\n` +
    'Connection: close\r\n' +
    '\r\n' +
    payload
  );
  socket.destroy();

  const clientIp = getClientIp(req);
  console.warn(`[ws-upgrade] ${statusCode} ${statusText} ${req.url || '/'} ip=${clientIp}`);
}

server.on('upgrade', (req, socket, head) => {
  const url = req.url || '/';
  const parsedUrl = new URL(url, `http://${req.headers.host || 'localhost'}`);
  const token = parsedUrl.searchParams.get('token') ?? undefined;

  if (url.startsWith('/ws/')) {
    const room = parsedUrl.pathname.slice(4);
    if (WS_AUTH_ENFORCE && !verifyWsJoinToken({ token, room, scope: 'chat', secret: WS_AUTH_SECRET })) {
      rejectUpgrade(req, socket, 401, 'Unauthorized', 'Invalid or missing websocket token');
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => {
      chatHandler(ws, room, state);
    });
  } else if (url.startsWith('/invite-ws/')) {
    const match = url.match(/^\/invite-ws\/([^?]*)/);
    if (match) {
      const room = match[1];
      if (WS_AUTH_ENFORCE && !verifyWsJoinToken({ token, room, scope: 'invite', secret: WS_AUTH_SECRET })) {
        rejectUpgrade(req, socket, 401, 'Unauthorized', 'Invalid or missing websocket token');
        return;
      }
      const query = parsedUrl.searchParams;
      const inviteQuery = {
        limit: query.get('limit') ? parseInt(query.get('limit'), 10) : undefined,
        creator: query.get('creator'),
      };
      wss.handleUpgrade(req, socket, head, (ws) => {
        inviteHandler(ws, room, state, inviteQuery);
      });
    }
  } else {
    rejectUpgrade(req, socket, 404, 'Not Found', 'WebSocket endpoint not found');
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
  const isCreatorClaim = isCreatorFlag(query.creator);
  const currentCreatorConnId = state.inviteRoomCreators.get(room);
  const hasActiveCreator = !!(currentCreatorConnId && roomMap.has(currentCreatorConnId));
  const isCreator = isCreatorClaim && !hasActiveCreator;

  if (isCreator) {
    state.inviteRoomCreators.set(room, 'pending');
  }

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
  const existingConnIds = Array.from(roomMap.keys());
  roomMap.set(connId, ws);

  if (isCreator) {
    state.inviteRoomCreators.set(room, connId);
  }

  for (const existingConnId of existingConnIds) {
    if (ws.readyState === 1) {
      ws.send(JSON.stringify({
        type: 'invite_accepted',
        by: existingConnId,
        isCreator: false,
      }));
    }
  }

  const notice = {
    type: 'invite_accepted',
    by: connId,
    isCreator: isCreator,
  };
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
    if (state.inviteRoomCreators.get(room) === connId) {
      state.inviteRoomCreators.delete(room);
    }
    if (roomMap.size === 0) {
      state.inviteRooms.delete(room);
      state.inviteRoomLimits.delete(room);
      state.inviteRoomCreators.delete(room);
    }
    console.log(`[invite-ws] connection ${connId} left room ${room}`);
  });

  ws.on('error', (err) => {
    console.error(`[invite-ws] error:`, err);
  });
}

server.listen(port, host, () => {
  console.log(`listening on ${host}:${port}`);
});
