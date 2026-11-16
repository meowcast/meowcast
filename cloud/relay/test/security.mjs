// Isolated workerd regression tests. Never connects to the deployed relay/dongle.
import assert from 'node:assert/strict';
import WS from 'ws';
import { build } from 'esbuild';
import * as runtime from 'miniflare';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';

const bundle = await build({ entryPoints: [new URL('../src/index.ts', import.meta.url).pathname], bundle: true, format: 'esm', platform: 'neutral', external: ['cloudflare:workers'], write: false });
const options = { modules: true, script: bundle.outputFiles[0].text, compatibilityDate: '2026-10-28', durableObjects: { DEVICE: { className: 'Device', useSQLite: true } }, bindings: { PILOT_ORIGINS: 'https://pilot.example' }, cf: false };
const mf = new runtime.Miniflare(runtime.convertV4MiniflareOptions ? runtime.convertV4MiniflareOptions(options) : options);
const sockets = [];
const id = 'a'.repeat(32), token = 'c'.repeat(64);
const hash = createHash('sha256').update(token).digest('hex');
const upgrade = (path, headers = {}) => mf.dispatchFetch(`http://audit.local${path}`, { headers: { Upgrade: 'websocket', ...headers } });
const auth = { Authorization: `Bearer ${token}` };
const pause = (ms) => new Promise((r) => setTimeout(r, ms));
function event(ws, type, predicate = () => true, timeout = 3000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { ws.removeEventListener(type, listener); reject(Error(`timeout waiting for ${type}`)); }, timeout);
    const listener = (e) => { if (predicate(e)) { clearTimeout(timer); ws.removeEventListener(type, listener); resolve(e); } };
    ws.addEventListener(type, listener);
  });
}
async function liveUpgrade(path, headers = {}) {
  const base = (await mf.ready).toString().replace(/^http/, 'ws').replace(/\/$/, '');
  const ws = new WS(base + path, { headers });
  ws.binaryType = 'arraybuffer';
  await new Promise((resolve, reject) => { ws.once('open', resolve); ws.once('error', reject); });
  sockets.push(ws);
  return ws;
}
let device;
async function policy(enabled, generation = 1, hashes = [hash], lease = 15000) {
  device.send(JSON.stringify({ t: 'debug-policy', enabled, generation, hashes, lease_ms: lease }));
  // A ping round trip establishes that the policy has been processed.
  const pong = event(device, 'message', (e) => typeof e.data === 'string' && JSON.parse(e.data).t === 'pong');
  device.send(JSON.stringify({ t: 'ping' }));
  await pong;
}
async function connect(ready = true) {
  const opened = event(device, 'message', (e) => typeof e.data === 'string' && JSON.parse(e.data).t === 'tunnel-open');
  const ws = await liveUpgrade(`/dev/${id}`, auth);
  const { cid } = JSON.parse((await opened).data);
  if (ready) {
    const acknowledged = event(ws, 'message', (e) => typeof e.data === 'string' && JSON.parse(e.data).t === 'ready');
    device.send(JSON.stringify({ t: 'tunnel-ready', cid }));
    await acknowledged;
  }
  return { ws, cid };
}
try {
  device = await liveUpgrade(`/device/${id}`, { 'X-Device-Key': 'b'.repeat(64) });
  assert.equal((await upgrade(`/dev/${id}`)).status, 401);
  assert.equal((await upgrade(`/dev/${id}`, auth)).status, 403);
  await policy(true);
  assert.equal((await upgrade(`/dev/${id}`, { ...auth, Origin: 'https://attacker.example' })).status, 403);
  assert.equal((await upgrade(`/dev/${id}`, { Authorization: `Bearer ${'d'.repeat(64)}` })).status, 403);

  console.log('checking authenticated forwarding and capacity');
  let first = await connect();
  const second = await connect();
  assert.equal((await upgrade(`/dev/${id}`, auth)).status, 429);
  const bytes = event(device, 'message', (e) => typeof e.data !== 'string');
  first.ws.send(new Uint8Array([1, 2, 3]).buffer);
  const received = (await bytes).data;
  assert.equal(new DataView(received).getUint32(0), first.cid);
  assert.deepEqual([...new Uint8Array(received).slice(4)], [1, 2, 3]);
  const reply = event(first.ws, 'message', (e) => typeof e.data !== 'string');
  device.send(received);
  assert.deepEqual([...new Uint8Array((await reply).data)], [1, 2, 3]);
  console.log('checking oversized frame isolation');
  const oversized = event(first.ws, 'close');
  first.ws.send(new Uint8Array(65533).buffer);
  assert.equal((await oversized).code, 1009);
  const online = await mf.dispatchFetch(`http://audit.local/api/info?device=${id}`);
  assert.equal((await online.json()).online, true, 'oversize input must leave the device connection alive');
  console.log('checking credential revocation');
  const revoked = event(second.ws, 'message', e => typeof e.data === 'string' && JSON.parse(e.data).t === 'closed');
  await policy(true, 2, []);
  assert.equal(JSON.parse((await revoked).data).code, 4403);
  second.ws.terminate();
  assert.equal((await upgrade(`/dev/${id}`, auth)).status, 403);

  await policy(true, 3);
  first = await connect(false);
  console.log('checking pre-approval data');
  const premature = event(first.ws, 'close');
  first.ws.send(new Uint8Array([1]).buffer);
  assert.equal((await premature).code, 4403, 'data requires dongle approval');
  await pause(10);
  first = await connect();
  console.log('checking disabled debugging');
  const disabled = event(first.ws, 'message', e => typeof e.data === 'string' && JSON.parse(e.data).t === 'closed');
  await policy(false, 4);
  assert.equal(JSON.parse((await disabled).data).code, 4403);
  first.ws.terminate();
  assert.equal((await upgrade(`/dev/${id}`, auth)).status, 403);

  await policy(true, 5, [hash], 1);
  await pause(10);
  assert.equal((await upgrade(`/dev/${id}`, auth)).status, 403, 'stale policy must fail closed');
  await policy(true, 6);
  first = await connect();
  const limited = event(first.ws, 'message', e => typeof e.data === 'string' && JSON.parse(e.data).t === 'closed');
  for (let i = 0; i < 80; i++) first.ws.send(new Uint8Array(16384).buffer);
  assert.equal(JSON.parse((await limited).data).code, 4429);
  first.ws.terminate();
  await policy(true, 7, [hash], 1000);
  first = await connect();
  const expired = event(first.ws, 'message', e => typeof e.data === 'string' && JSON.parse(e.data).t === 'closed', 7000);
  assert.equal(JSON.parse((await expired).data).code, 4403, 'idle sockets must be swept after policy expiry');
  first.ws.terminate();
  console.log('PASS: anonymous admission, origin, credentials, capacity, bidirectional bytes, oversize isolation, revocation, readiness, off switch, rate limit, admission and active policy expiry');
  for (const script of ['e2e.mjs', 'reconnect.mjs', 'tunnel.mjs']) {
    const base = (await mf.ready).toString().replace(/\/$/, '');
    await new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [new URL(script, import.meta.url).pathname, base], { stdio: 'inherit' });
      child.on('error', reject);
      child.on('exit', (code) => code === 0 ? resolve() : reject(Error(`${script} exited with ${code}`)));
    });
  }
} finally {
  for (const ws of sockets) try { ws.close(1000); } catch {}
  await mf.dispose();
}
