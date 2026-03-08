import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import net from 'node:net';
import { setTimeout as sleep } from 'node:timers/promises';
import WebSocket from 'ws';
import { createWsJoinToken } from '../src/serverUtils.js';

function pickPort() {
  return 20000 + Math.floor(Math.random() * 10000);
}

async function canBindLocalPort() {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.once('error', () => resolve(false));
    server.listen(0, '127.0.0.1', () => {
      server.close(() => resolve(true));
    });
  });
}

function waitForMessage(ws, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error('Timed out waiting for websocket message'));
    }, timeoutMs);
    ws.once('message', (data) => {
      clearTimeout(timer);
      resolve(String(data));
    });
    ws.once('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

function waitForOpen(ws, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Timed out waiting for websocket open')), timeoutMs);
    ws.once('open', () => {
      clearTimeout(timer);
      resolve();
    });
    ws.once('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

function waitForUnexpectedResponse(ws, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Timed out waiting for websocket rejection')), timeoutMs);
    ws.once('unexpected-response', (req, res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        body += chunk;
      });
      res.on('end', () => {
        clearTimeout(timer);
        resolve({ statusCode: res.statusCode, body });
      });
    });
    ws.once('open', () => {
      clearTimeout(timer);
      reject(new Error('Expected websocket rejection but connection opened'));
    });
    ws.once('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

test('websocket relay and rate limiting work end-to-end', async (t) => {
  if (!(await canBindLocalPort())) {
    t.skip('Local socket bind is not permitted in this environment');
    return;
  }

  const port = pickPort();
  const serverProc = spawn('node', ['src/main.js'], {
    cwd: process.cwd(),
    env: {
      ...process.env,
      BIND_ADDR: `127.0.0.1:${port}`,
      WS_RATE_LIMIT: '2',
      WS_WINDOW_SECS: '60',
      HTTP_RATE_LIMIT: '9999',
      HTTP_WINDOW_SECS: '60',
      RATE_LIMIT_REDIS_REST_URL: '',
      RATE_LIMIT_REDIS_REST_TOKEN: '',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  try {
    await Promise.race([
      new Promise((resolve, reject) => {
        serverProc.stdout.on('data', (chunk) => {
          const text = String(chunk);
          if (text.includes(`listening on 127.0.0.1:${port}`)) {
            resolve();
          }
        });
        serverProc.once('exit', (code) => reject(new Error(`Server exited early with code ${code}`)));
      }),
      sleep(5000).then(() => {
        throw new Error('Server did not start in time');
      }),
    ]);

    const ws1 = new WebSocket(`ws://127.0.0.1:${port}/ws/test-room`);
    const ws2 = new WebSocket(`ws://127.0.0.1:${port}/ws/test-room`);
    await Promise.all([waitForOpen(ws1), waitForOpen(ws2)]);

    ws1.send(JSON.stringify({ ciphertext: 'hello-1' }));
    const relayedRaw = await waitForMessage(ws2);
    const relayed = JSON.parse(relayedRaw);
    assert.equal(relayed.ciphertext, 'hello-1');

    ws1.send(JSON.stringify({ ciphertext: 'hello-2' }));
    ws1.send(JSON.stringify({ ciphertext: 'hello-3' }));
    const rateLimitedRaw = await waitForMessage(ws1);
    const rateLimited = JSON.parse(rateLimitedRaw);
    assert.equal(rateLimited.error, 'rate_limited');

    ws1.close();
    ws2.close();
  } finally {
    serverProc.kill('SIGTERM');
  }
});

test('invite creator receives accepted state even when joining after peer', async (t) => {
  if (!(await canBindLocalPort())) {
    t.skip('Local socket bind is not permitted in this environment');
    return;
  }

  const port = pickPort();
  const serverProc = spawn('node', ['src/main.js'], {
    cwd: process.cwd(),
    env: {
      ...process.env,
      BIND_ADDR: `127.0.0.1:${port}`,
      WS_RATE_LIMIT: '10',
      WS_WINDOW_SECS: '60',
      HTTP_RATE_LIMIT: '9999',
      HTTP_WINDOW_SECS: '60',
      RATE_LIMIT_REDIS_REST_URL: '',
      RATE_LIMIT_REDIS_REST_TOKEN: '',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  try {
    await Promise.race([
      new Promise((resolve, reject) => {
        serverProc.stdout.on('data', (chunk) => {
          const text = String(chunk);
          if (text.includes(`listening on 127.0.0.1:${port}`)) {
            resolve();
          }
        });
        serverProc.once('exit', (code) => reject(new Error(`Server exited early with code ${code}`)));
      }),
      sleep(5000).then(() => {
        throw new Error('Server did not start in time');
      }),
    ]);

    const roomId = 'late-creator-room';
    const peerWs = new WebSocket(`ws://127.0.0.1:${port}/invite-ws/${roomId}?limit=2`);
    await waitForOpen(peerWs);

    const creatorWs = new WebSocket(`ws://127.0.0.1:${port}/invite-ws/${roomId}?limit=2&creator=1`);
    const creatorMessagePromise = waitForMessage(creatorWs);
    await waitForOpen(creatorWs);

    const creatorNotice = JSON.parse(await creatorMessagePromise);
    assert.equal(creatorNotice.type, 'invite_accepted');
    assert.ok(typeof creatorNotice.by === 'string' && creatorNotice.by.length > 0);

    peerWs.close();
    creatorWs.close();
  } finally {
    serverProc.kill('SIGTERM');
  }
});

test('websocket upgrade returns 401 without a valid token in production', async (t) => {
  if (!(await canBindLocalPort())) {
    t.skip('Local socket bind is not permitted in this environment');
    return;
  }

  const port = pickPort();
  const secret = 'integration-secret';
  const serverProc = spawn('node', ['src/main.js'], {
    cwd: process.cwd(),
    env: {
      ...process.env,
      NODE_ENV: 'production',
      WS_AUTH_SECRET: secret,
      BIND_ADDR: `127.0.0.1:${port}`,
      WS_RATE_LIMIT: '10',
      WS_WINDOW_SECS: '60',
      HTTP_RATE_LIMIT: '9999',
      HTTP_WINDOW_SECS: '60',
      RATE_LIMIT_REDIS_REST_URL: '',
      RATE_LIMIT_REDIS_REST_TOKEN: '',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  try {
    await Promise.race([
      new Promise((resolve, reject) => {
        serverProc.stdout.on('data', (chunk) => {
          const text = String(chunk);
          if (text.includes(`listening on 127.0.0.1:${port}`)) {
            resolve();
          }
        });
        serverProc.once('exit', (code) => reject(new Error(`Server exited early with code ${code}`)));
      }),
      sleep(5000).then(() => {
        throw new Error('Server did not start in time');
      }),
    ]);

    const rejectedWs = new WebSocket(`ws://127.0.0.1:${port}/ws/protected-room`);
    const rejection = await waitForUnexpectedResponse(rejectedWs);
    assert.equal(rejection.statusCode, 401);
    assert.match(rejection.body, /Invalid or missing websocket token/);

    const token = createWsJoinToken({
      room: 'protected-room',
      scope: 'chat',
      secret,
      ttlSeconds: 120,
    });
    const acceptedWs = new WebSocket(`ws://127.0.0.1:${port}/ws/protected-room?token=${token}`);
    await waitForOpen(acceptedWs);
    acceptedWs.close();
  } finally {
    serverProc.kill('SIGTERM');
  }
});
