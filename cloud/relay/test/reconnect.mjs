// Same device reconnects: debug admission uses the new connection and fresh policy.
import assert from 'node:assert/strict';
import WS from 'ws';
import { randomBytes, createHash } from 'node:crypto';
const base = (process.argv[2] ?? 'http://localhost:8787').replace(/^http/, 'ws').replace(/\/$/, '');
const id = randomBytes(16).toString('hex'), key = randomBytes(32).toString('hex');
const token = randomBytes(32).toString('hex'), hash = createHash('sha256').update(token).digest('hex');
setTimeout(() => { console.log('FAIL timeout'); process.exit(1); }, 15000);
const connect = () => new Promise((r, j) => {
  const w = new WS(`${base}/device/${id}`, { headers: { 'X-Device-Key': key } });
  w.on('open', () => r(w)); w.on('error', j);
});
const a = await connect();
await new Promise((r) => setTimeout(r, 50));
const b = await connect();
let newConnectionUsed = false;
b.on('message', (data, binary) => {
  if (binary) return;
  const m = JSON.parse(data);
  if (m.t === 'tunnel-open') {
    assert.equal(m.hash, hash);
    newConnectionUsed = true;
    b.send(JSON.stringify({ t: 'tunnel-ready', cid: m.cid }));
  }
});
const pong = new Promise((resolve) => b.on('message', data => { if (JSON.parse(data).t === 'pong') resolve(); }));
b.send(JSON.stringify({ t: 'debug-policy', enabled: true, hashes: [hash], generation: 1, lease_ms: 15000 }));
b.send(JSON.stringify({ t: 'ping' }));
await pong;
const c = new WS(`${base}/dev/${id}`, { headers: { Authorization: `Bearer ${token}` } });
c.on('error', (e) => { console.error(e); process.exit(1); });
c.on('message', data => {
  if (JSON.parse(data).t === 'ready') {
    assert.equal(newConnectionUsed, true);
    console.log('OK: reconnected device approved the tunnel');
    process.exit(0);
  }
});
