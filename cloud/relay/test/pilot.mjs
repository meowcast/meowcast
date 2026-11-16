// Pilot protocol 2 through the relay (wrangler dev): envelopes, presence, and compatibility
// with a dongle or a pilot that only knows keys.
// node test/pilot.mjs [http://localhost:8787]
import assert from 'node:assert/strict';
import WS from 'ws';
import { createHash, randomBytes } from 'node:crypto';
setTimeout(() => { console.log('FAIL timeout'); process.exit(1); }, 30000);
const base = (process.argv[2] ?? 'http://localhost:8787').replace(/\/$/, '');
const wsBase = base.replace(/^http/, 'ws');
const sha = (s) => createHash('sha256').update(s).digest('hex');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, f) => { for (let i = 0; i < 100; i++) { if (f()) return; await sleep(30); } assert.fail(`timed out waiting for ${what}`); };

// A dongle. `v2`: it speaks the pilot protocol; otherwise it is the firmware flashed before it
// (keys, pairing and status only, everything else ignored).
async function dongle(v2) {
  const id = randomBytes(16).toString('hex');
  const ws = new WS(`${wsBase}/device/${id}`, { headers: { 'X-Device-Key': randomBytes(32).toString('hex') } });
  await new Promise((r, j) => { ws.on('open', r); ws.on('error', j); });
  const d = { id, ws, got: [], keys: [], tokens: new Set(), pids: [] };
  ws.on('message', (data) => {
    const m = JSON.parse(data);
    d.got.push(m);
    if (m.t === 'pair') {
      const token = randomBytes(16).toString('base64url');
      d.tokens.add(sha(token));
      ws.send(JSON.stringify({ t: 'pair-result', rid: m.rid, ok: true, token }));
    } else if (m.t === 'hello') {
      const ok = d.tokens.has(m.hash);
      ws.send(JSON.stringify({ t: 'auth', pid: m.pid, ok }));
      if (ok) d.pids.push(m.pid);
      if (ok && v2) ws.send(JSON.stringify({ t: 'pilot', pid: m.pid, m: { t: 'state', v: 2, caps: ['vol'], vol: 40 } }));
    } else if (m.t === 'key') d.keys.push(m.k);
    else if (m.t === 'msg' && v2 && m.m.t === 'ping') ws.send(JSON.stringify({ t: 'pilot', pid: m.pid, m: { t: 'pong', n: m.m.n } }));
  });
  ws.send(JSON.stringify({ t: 'status', dongle: true }));
  return d;
}

async function pair(d) {
  const r = await fetch(`${base}/api/pair`, { method: 'POST', body: JSON.stringify({ device: d.id, code: '123456', name: 'test' }) });
  const b = await r.json();
  assert.equal(b.ok, true);
  return b.token;
}

async function phone(d, token) {
  const ws = new WS(`${wsBase}/pilot/${d.id}?k=${token}`);
  const p = { ws, got: [], closed: null };
  ws.on('message', (data) => p.got.push(JSON.parse(data)));
  ws.on('close', (code) => { p.closed = code; });
  await new Promise((r, j) => { ws.on('open', r); ws.on('error', j); });
  return p;
}

// ---- new dongle, new pilots ----------------------------------------------------------
{
  const d = await dongle(true);
  const a = await phone(d, await pair(d));
  const b = await phone(d, await pair(d));
  await until('both phones authorised', () => d.pids.length === 2);
  const [pidA, pidB] = d.pids;
  await until('state after auth', () => a.got.some((m) => m.t === 'state') && b.got.some((m) => m.t === 'state'));
  assert.deepEqual(a.got.find((m) => m.t === 'state'), { t: 'state', v: 2, caps: ['vol'], vol: 40 });
  assert.equal(a.got[0].t, 'status', 'the status line still comes first');

  // phone -> dongle: wrapped, with the sender's connection id
  a.ws.send(JSON.stringify({ t: 'vol', d: 5 }));
  await until('msg at the dongle', () => d.got.some((m) => m.t === 'msg' && m.m.t === 'vol'));
  assert.deepEqual(d.got.find((m) => m.t === 'msg' && m.m.t === 'vol'), { t: 'msg', pid: pidA, m: { t: 'vol', d: 5 } });

  // dongle -> one phone (the reply to a ping), and not the other
  a.ws.send(JSON.stringify({ t: 'ping', n: 7 }));
  await until('pong', () => a.got.some((m) => m.t === 'pong'));
  assert.deepEqual(a.got.find((m) => m.t === 'pong'), { t: 'pong', n: 7 });
  assert.ok(!b.got.some((m) => m.t === 'pong'), 'a reply reached another phone');

  // dongle -> every phone
  d.ws.send(JSON.stringify({ t: 'pilot', m: { t: 'state', v: 2, caps: ['vol'], vol: 45 } }));
  await until('broadcast', () => a.got.some((m) => m.vol === 45) && b.got.some((m) => m.vol === 45));

  // A phone cannot forge the relay's own messages: they arrive wrapped, never bare.
  const before = d.got.length;
  a.ws.send(JSON.stringify({ t: 'hello', pid: 'forged', hash: [...d.tokens][0] }));
  a.ws.send(JSON.stringify({ t: 'pair', rid: 'x', code: '000000' }));
  a.ws.send(JSON.stringify({ t: 'tunnel-open', cid: 1, hash: 'x' }));
  a.ws.send(JSON.stringify({ t: 'bye', pid: pidB }));
  await until('forgeries wrapped', () => d.got.length >= before + 4);
  assert.ok(d.got.slice(before).every((m) => m.t === 'msg' && m.pid === pidA), JSON.stringify(d.got.slice(before)));

  // Keys keep their own path (and their length limit).
  a.ws.send(JSON.stringify({ t: 'key', k: 'home' }));
  a.ws.send(JSON.stringify({ t: 'key', k: 'play' }));
  a.ws.send(JSON.stringify({ t: 'key', k: 'x'.repeat(9) }));
  a.ws.send(JSON.stringify({ t: 'key', k: 'left' }));
  await until('keys', () => d.keys.length === 3);
  assert.deepEqual(d.keys, ['home', 'play', 'left']);

  // An unauthorised phone: nothing it sends reaches the dongle, and it gets no state.
  const evil = await phone(d, 'x'.repeat(22));
  evil.ws.send(JSON.stringify({ t: 'vol', set: 100 }));
  await until('bad token closed', () => evil.closed !== null);
  assert.equal(evil.closed, 4401);
  assert.ok(!d.got.some((m) => m.t === 'msg' && m.m.set === 100));
  assert.ok(!evil.got.some((m) => m.t === 'state'));
  assert.ok(!d.got.some((m) => m.t === 'bye'), 'bye for a phone that was never authorised');

  // Presence: the dongle hears when a phone leaves.
  b.ws.close();
  await until('bye', () => d.got.some((m) => m.t === 'bye'));
  assert.deepEqual(d.got.find((m) => m.t === 'bye'), { t: 'bye', pid: pidB });

  // "Unpair phones": the dongle withdraws the authorisation, the phone has to pair again.
  d.ws.send(JSON.stringify({ t: 'auth', pid: pidA, ok: false }));
  await until('unpaired phone told', () => a.got.some((m) => m.t === 'unpaired'));
  // From now on nothing it sends reaches the dongle, whatever the close handshake does.
  a.ws.send(JSON.stringify({ t: 'key', k: 'up' }));
  await until('unpaired phone closed', () => a.closed !== null);
  assert.equal(a.closed, 4401);
  assert.ok(!d.keys.includes('up'));

  // The app list may be longer than a phone's messages.
  const c = await phone(d, await pair(d));
  await until('third phone', () => d.pids.length === 3);
  const apps = Array.from({ length: 24 }, (_, i) => ({ id: `dev.meow.example.application${i}`, name: 'An application with a long name '.repeat(4), st: 'installed' }));
  assert.ok(JSON.stringify(apps).length > 4096);
  d.ws.send(JSON.stringify({ t: 'pilot', m: { t: 'apps', apps } }));
  await until('long app list', () => c.got.some((m) => m.t === 'apps'));
  // ... and a phone's oversized message is dropped.
  c.ws.send(JSON.stringify({ t: 'text', s: 'x'.repeat(5000) }));
  c.ws.send(JSON.stringify({ t: 'text', s: 'ok' }));
  await until('text', () => d.got.some((m) => m.t === 'msg' && m.m.s === 'ok'));
  assert.ok(!d.got.some((m) => m.t === 'msg' && m.m.t === 'text' && m.m.s.length > 100));
  d.ws.close(); c.ws.close();
  console.log('OK new dongle + new pilot');
}

// ---- the firmware flashed today + a new pilot ------------------------------------------
{
  const d = await dongle(false);
  const a = await phone(d, await pair(d));
  await until('authorised', () => a.got.some((m) => m.t === 'status'));
  assert.deepEqual(a.got, [{ t: 'status', dongle: true, online: true }]);
  // The new pilot pings and asks for things the old dongle ignores; keys still work.
  a.ws.send(JSON.stringify({ t: 'ping', n: 1 }));
  a.ws.send(JSON.stringify({ t: 'key', k: 'right' }));
  a.ws.send(JSON.stringify({ t: 'key', k: 'home' }));
  await until('keys', () => d.keys.length === 2);
  assert.deepEqual(d.keys, ['right', 'home']);
  await sleep(200);
  assert.deepEqual(a.got, [{ t: 'status', dongle: true, online: true }], 'an old dongle announces nothing: the pilot hides the new controls');
  assert.equal(a.closed, null);
  // The dongle goes away: the phone hears it.
  d.ws.close();
  await until('offline status', () => a.got.some((m) => m.online === false));
  a.ws.close();
  console.log('OK old dongle + new pilot');
}

// ---- a new dongle + the pilot deployed today (keys only) --------------------------------
{
  const d = await dongle(true);
  const a = await phone(d, await pair(d));
  await until('authorised', () => a.got.some((m) => m.t === 'status'));
  a.ws.send(JSON.stringify({ t: 'key', k: 'ok' }));
  await until('key', () => d.keys.length === 1);
  a.ws.close();
  await until('bye', () => d.got.some((m) => m.t === 'bye'));
  d.ws.close();
  console.log('OK new dongle + old pilot');
}
console.log('ALL OK');
process.exit(0);
