export const DEFAULT_INVITE_ROOM_LIMIT = 2;
const MAX_INVITE_ROOM_LIMIT = 50;

export function readEnv(name, fallback) {
  const value = process.env[name];
  if (value === undefined) return fallback;
  const parsed = parseInt(value, 10);
  return !Number.isNaN(parsed) && parsed > 0 ? parsed : fallback;
}

export function isValidRoom(room, maxLen) {
  if (!room || room.length === 0 || room.length > maxLen) {
    return false;
  }
  return /^[a-zA-Z0-9\-_:]+$/.test(room);
}

export function normalizeInviteLimit(limit) {
  if (limit === undefined) return DEFAULT_INVITE_ROOM_LIMIT;
  return Math.max(
    DEFAULT_INVITE_ROOM_LIMIT,
    Math.min(limit, MAX_INVITE_ROOM_LIMIT)
  );
}

export function isCreatorFlag(raw) {
  return raw === '1' || raw === 'true' || raw === 'yes';
}

function parsePort(raw, fallbackPort) {
  const parsed = parseInt(raw, 10);
  if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
    return fallbackPort;
  }
  return parsed;
}

export function parseBindAddr(bindAddr, fallback = '0.0.0.0:8080') {
  const fallbackParsed = parseBindAddrCandidate(fallback, '0.0.0.0', 8080);
  return parseBindAddrCandidate(
    bindAddr || fallback,
    fallbackParsed.host,
    fallbackParsed.port
  );
}

export function validateServerConfig() {
  const bindAddr = process.env.BIND_ADDR ?? '0.0.0.0:8080';
  parseBindAddr(bindAddr, '0.0.0.0:8080');

  const redisUrl = process.env.RATE_LIMIT_REDIS_REST_URL?.trim();
  const redisToken = process.env.RATE_LIMIT_REDIS_REST_TOKEN?.trim();
  if ((redisUrl && !redisToken) || (!redisUrl && redisToken)) {
    throw new Error('RATE_LIMIT_REDIS_REST_URL and RATE_LIMIT_REDIS_REST_TOKEN must be set together');
  }

  if (process.env.WS_AUTH_ENFORCE === '1' && !process.env.WS_AUTH_SECRET?.trim()) {
    throw new Error('WS_AUTH_SECRET is required when WS_AUTH_ENFORCE=1');
  }
  if (process.env.NODE_ENV === 'production' && !process.env.WS_AUTH_SECRET?.trim()) {
    throw new Error('WS_AUTH_SECRET is required in production');
  }
}

function base64UrlEncode(input) {
  return Buffer.from(input).toString('base64url');
}

function base64UrlDecode(input) {
  return Buffer.from(input, 'base64url');
}

function signPayload(payloadB64, secret) {
  return createHmac('sha256', secret).update(payloadB64).digest('base64url');
}

export function verifyWsJoinToken({ token, room, scope, secret }) {
  if (!secret) return false;
  if (!token || typeof token !== 'string') return false;

  const parts = token.split('.');
  if (parts.length !== 2) return false;
  const [payloadB64, sigB64] = parts;
  if (!payloadB64 || !sigB64) return false;

  const expectedSig = signPayload(payloadB64, secret);
  const providedSig = sigB64;
  const expectedBuf = Buffer.from(expectedSig);
  const providedBuf = Buffer.from(providedSig);
  if (expectedBuf.length !== providedBuf.length) return false;
  if (!timingSafeEqual(expectedBuf, providedBuf)) return false;

  try {
    const payloadRaw = base64UrlDecode(payloadB64).toString('utf8');
    const payload = JSON.parse(payloadRaw);
    if (payload.room !== room) return false;
    if (payload.scope !== scope) return false;
    if (!Number.isFinite(payload.exp) || payload.exp < Math.floor(Date.now() / 1000)) return false;
    return true;
  } catch {
    return false;
  }
}

export function createWsJoinToken({ room, scope, secret, ttlSeconds = 120 }) {
  const exp = Math.floor(Date.now() / 1000) + ttlSeconds;
  const payloadB64 = base64UrlEncode(JSON.stringify({ room, scope, exp }));
  const sigB64 = signPayload(payloadB64, secret);
  return `${payloadB64}.${sigB64}`;
}

function parseBindAddrCandidate(value, fallbackHost, fallbackPort) {
  const input = String(value || '').trim();
  if (!input) {
    return { host: fallbackHost, port: fallbackPort };
  }

  // [IPv6]:port
  const ipv6Match = input.match(/^\[([^\]]+)\]:(\d{1,5})$/);
  if (ipv6Match) {
    const host = ipv6Match[1].trim();
    return { host: host || fallbackHost, port: parsePort(ipv6Match[2], fallbackPort) };
  }

  // port-only shorthand
  if (/^\d{1,5}$/.test(input)) {
    return { host: fallbackHost, port: parsePort(input, fallbackPort) };
  }

  // host:port only when there is a single ":" separator (avoids IPv6 ambiguity).
  const firstColon = input.indexOf(':');
  const lastColon = input.lastIndexOf(':');
  if (firstColon > 0 && firstColon === lastColon) {
    const host = input.slice(0, firstColon).trim();
    const portRaw = input.slice(firstColon + 1).trim();
    if (host && /^\d{1,5}$/.test(portRaw)) {
      return { host, port: parsePort(portRaw, fallbackPort) };
    }
  }

  // host-only (including raw IPv6 without brackets).
  return { host: input, port: fallbackPort };
}
import { createHmac, timingSafeEqual } from 'crypto';
