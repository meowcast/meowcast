import WS from 'ws';
// End-to-end check of the relay (wrangler dev): a fake dongle and a fake phone.
// node test/e2e.mjs [http://localhost:8787]
import { createHash, randomBytes } from 'node:crypto';
setTimeout(() => { console.log('FAIL timeout'); process.exit(1); }, 30000);
const base = process.argv[2] ?? 'http://localhost:8787';
const wsBase = base.replace(/^http/, 'ws');
const id = randomBytes(16).toString('hex');
const key = randomBytes(32).toString('hex');
const sha = (s) => createHash('sha256').update(s).digest('hex');
const tokens = new Set();
const log = (...a) => console.log(...a);
const fail = (m) => { console.log('FAIL', m); process.exit(1); };
const open = (url, opts) => new Promise((res, rej) => { const w = new WebSocket(url, opts); w.onopen = () => res(w); w.onerror = (e) => rej(e); });

// 1. Pairing with no device online -> 503 offline
let r = await fetch(`${base}/api/pair`, { method: 'POST', body: JSON.stringify({ device: id, code: '123456' }) });
log('pair offline', r.status, await r.text());
if (r.status !== 503) fail('expected 503');

// 2. Device connects (needs a header: node's WebSocket has no headers option -> use undici's)
const UWS = WS;
const dev = await new Promise((res, rej) => { const w = new UWS(`${wsBase}/device/${id}`, { headers: { 'X-Device-Key': key } }); w.onopen = () => res(w); w.onerror = rej; });
const keys = [];
dev.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.t === 'pair') {
    const ok = m.code === '654321';
    const token = randomBytes(16).toString('base64url');
    if (ok) tokens.add(sha(token));
    dev.send(JSON.stringify(ok ? { t: 'pair-result', rid: m.rid, ok, token } : { t: 'pair-result', rid: m.rid, ok, error: 'bad_code' }));
  } else if (m.t === 'hello') {
    dev.send(JSON.stringify({ t: 'auth', pid: m.pid, ok: tokens.has(m.hash) }));
  } else if (m.t === 'key') keys.push(m.k);
};
dev.send(JSON.stringify({ t: 'status', dongle: true }));
log('device connected');

// 3. Wrong key for the same id is refused
const bad = await new Promise((res) => { const w = new UWS(`${wsBase}/device/${id}`, { headers: { 'X-Device-Key': '0'.repeat(64) } }); w.onopen = () => res('opened'); w.onerror = () => res('refused'); });
log('impostor device', bad);
if (bad !== 'refused') fail('impostor accepted');

// 4. Pairing: wrong code, then right code
r = await fetch(`${base}/api/pair`, { method: 'POST', body: JSON.stringify({ device: id, code: '000000' }) });
log('pair wrong', r.status, await r.text());
r = await fetch(`${base}/api/pair`, { method: 'POST', body: JSON.stringify({ device: id, code: '654321', name: 'test' }) });
const pr = await r.json();
log('pair right', r.status, pr.ok);
if (!pr.ok) fail('pairing failed');

// 5. Pilot with the token: status, keys arrive at the device
const pilot = await open(`${wsBase}/pilot/${id}?k=${pr.token}`);
const msgs = [];
pilot.onmessage = (ev) => msgs.push(JSON.parse(ev.data));
await new Promise((r) => setTimeout(r, 400));
pilot.send(JSON.stringify({ t: 'key', k: 'right' }));
pilot.send(JSON.stringify({ t: 'key', k: 'ok' }));
await new Promise((r) => setTimeout(r, 400));
log('pilot got', JSON.stringify(msgs), 'device got keys', JSON.stringify(keys));
if (keys.join() !== 'right,ok') fail('keys not relayed');

// 6. Pilot with a bad token is closed with 4401, and its keys never arrive
const evil = await open(`${wsBase}/pilot/${id}?k=${'x'.repeat(22)}`);
const code = await new Promise((res) => { evil.onclose = (e) => res(e.code); evil.send(JSON.stringify({ t: 'key', k: 'left' })); });
log('bad pilot closed', code, 'keys', JSON.stringify(keys));
if (code !== 4401 || keys.includes('left')) fail('bad token not rejected');

// 7. info
r = await fetch(`${base}/api/info?device=${id}`);
log('info', await r.text());
dev.close(); pilot.close();
await new Promise((r) => setTimeout(r, 300));
r = await fetch(`${base}/api/info?device=${id}`);
log('info after device left', await r.text());
log('ALL OK');
process.exit(0);
