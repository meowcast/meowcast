// Authenticated tunnel check: fake dongle + developer client; bytes flow both ways.
import WS from 'ws';
import { randomBytes, createHash } from 'node:crypto';
const base = (process.argv[2] ?? 'http://localhost:8787').replace(/^http/, 'ws');
const id = randomBytes(16).toString('hex'), key = randomBytes(32).toString('hex');
const token = randomBytes(32).toString('hex');
const hash = createHash('sha256').update(token).digest('hex');
setTimeout(() => { console.log('FAIL timeout'); process.exit(1); }, 15000);
const dev = new WS(`${base}/device/${id}`, { headers: { 'X-Device-Key': key } });
await new Promise((r, j) => { dev.on('open', r); dev.on('error', j); });
const policyReady = new Promise((resolve) => {
  dev.on('message', (d, isBin) => {
    if (!isBin) {
      const m = JSON.parse(d);
      if (m.t === 'pong') resolve();
      if (m.t === 'tunnel-open') {
        if (m.hash !== hash) throw Error('incorrect admission hash');
        dev.send(JSON.stringify({ t: 'tunnel-ready', cid: m.cid }));
      }
      return;
    }
    const b = Buffer.from(d);
    dev.send(Buffer.concat([b.subarray(0, 4), Buffer.from('SSH-2.0-fake\r\n')]), { binary: true });
  });
});
dev.send(JSON.stringify({ t: 'debug-policy', enabled: true, hashes: [hash], generation: 1, lease_ms: 15000 }));
dev.send(JSON.stringify({ t: 'ping' }));
await policyReady;
const c = new WS(`${base}/dev/${id}`, { headers: { Authorization: `Bearer ${token}` } });
c.on('message', (d, binary) => {
  if (!binary) {
    if (JSON.parse(d).t === 'ready') c.send(Buffer.from('hello'), { binary: true });
    return;
  }
  console.log('dev got', Buffer.from(d).toString().trim());
  console.log('OK');
  process.exit(0);
});
c.on('error', (e) => { console.error(e); process.exit(1); });
c.on('close', (code, r) => { console.log('dev closed', code, String(r)); process.exit(1); });
