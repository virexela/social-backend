import test from 'node:test';
import assert from 'node:assert/strict';
import {
  DEFAULT_INVITE_ROOM_LIMIT,
  isCreatorFlag,
  createWsJoinToken,
  isValidRoom,
  normalizeInviteLimit,
  parseBindAddr,
  validateServerConfig,
  verifyWsJoinToken,
} from '../src/serverUtils.js';

test('isValidRoom validates allowed format and length', () => {
  assert.equal(isValidRoom('room-1:abc_DEF', 128), true);
  assert.equal(isValidRoom('', 128), false);
  assert.equal(isValidRoom('bad room', 128), false);
  assert.equal(isValidRoom('x'.repeat(129), 128), false);
});

test('normalizeInviteLimit clamps values to supported range', () => {
  assert.equal(normalizeInviteLimit(undefined), DEFAULT_INVITE_ROOM_LIMIT);
  assert.equal(normalizeInviteLimit(1), DEFAULT_INVITE_ROOM_LIMIT);
  assert.equal(normalizeInviteLimit(2), 2);
  assert.equal(normalizeInviteLimit(99), 50);
});

test('isCreatorFlag accepts explicit creator values only', () => {
  assert.equal(isCreatorFlag('1'), true);
  assert.equal(isCreatorFlag('true'), true);
  assert.equal(isCreatorFlag('yes'), true);
  assert.equal(isCreatorFlag('TRUE'), false);
  assert.equal(isCreatorFlag('0'), false);
});

test('parseBindAddr parses host and port safely', () => {
  assert.deepEqual(parseBindAddr('0.0.0.0:8080'), { host: '0.0.0.0', port: 8080 });
  assert.deepEqual(parseBindAddr('[::1]:3001'), { host: '::1', port: 3001 });
  assert.deepEqual(parseBindAddr('9090'), { host: '0.0.0.0', port: 9090 });
  assert.deepEqual(parseBindAddr('localhost'), { host: 'localhost', port: 8080 });
  assert.deepEqual(parseBindAddr('localhost:not-a-port'), { host: 'localhost:not-a-port', port: 8080 });
});

test('validateServerConfig enforces redis rate-limit env pairing', () => {
  const oldUrl = process.env.RATE_LIMIT_REDIS_REST_URL;
  const oldToken = process.env.RATE_LIMIT_REDIS_REST_TOKEN;
  try {
    process.env.RATE_LIMIT_REDIS_REST_URL = 'https://example.com';
    delete process.env.RATE_LIMIT_REDIS_REST_TOKEN;
    assert.throws(() => validateServerConfig());

    process.env.RATE_LIMIT_REDIS_REST_TOKEN = 'token';
    assert.doesNotThrow(() => validateServerConfig());
  } finally {
    if (oldUrl === undefined) delete process.env.RATE_LIMIT_REDIS_REST_URL;
    else process.env.RATE_LIMIT_REDIS_REST_URL = oldUrl;
    if (oldToken === undefined) delete process.env.RATE_LIMIT_REDIS_REST_TOKEN;
    else process.env.RATE_LIMIT_REDIS_REST_TOKEN = oldToken;
  }
});

test('ws join token verification validates scope, room, and expiry', async () => {
  const token = createWsJoinToken({
    room: 'room-1',
    scope: 'chat',
    secret: 'secret',
    ttlSeconds: 1,
  });
  assert.equal(
    verifyWsJoinToken({ token, room: 'room-1', scope: 'chat', secret: 'secret' }),
    true
  );
  assert.equal(
    verifyWsJoinToken({ token, room: 'room-2', scope: 'chat', secret: 'secret' }),
    false
  );
  assert.equal(
    verifyWsJoinToken({ token, room: 'room-1', scope: 'invite', secret: 'secret' }),
    false
  );
});
