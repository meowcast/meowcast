// meowcast relay: connects the pilot web app (phone) and a dongle when they cannot reach each other
// directly (routers that isolate Wi-Fi clients, phone on mobile data, ...).
//
//   dongle  --wss /device/<id>  (X-Device-Key)-->  Device DO  <--wss /pilot/<id>?k=TOKEN--  phone
//   phone   --POST /api/pair {device, code, name}-->  Device DO --> dongle (checks the code)
//
// Pilot protocol 2 (volume, apps, text, heartbeat; apps/netd/src/pilot.rs) passes through
// in two envelopes, so new pilot features need no relay change:
//   phone -> relay {t: anything but "key", ...}  ->  dongle {t:"msg", pid, m: {...}}
//   dongle -> relay {t:"pilot", pid?, m: {...}}   ->  that phone, or every authorised phone: m
//   a phone's socket closes                       ->  dongle {t:"bye", pid}
// A dongle with older firmware ignores "msg" and "bye" and never sends "pilot": keys and the
// status line work as before, and the pilot app hides what the dongle did not announce.
//
// The relay persists no pilot secrets. Pairing codes and pilot tokens are checked by the dongle,
// which keeps the token hashes. The relay forwards SHA-256(token) for checking, never the token.
// A device registers on its first connection: the SHA-256 of its key is stored, and later
// connections must present the same key (trust on first use; ids are 128-bit random).
import { DurableObject } from 'cloudflare:workers';

const ID_RE = /^[0-9a-f]{32}$/;
const KEY_RE = /^[0-9a-f]{64}$/;
const MAX_MSG = 4096;
// The dongle is authenticated by its key; its app list for the pilots can be longer.
const MAX_DEVICE_MSG = 16_384;
const PAIR_TIMEOUT_MS = 10_000;

type PilotAttachment = { role: 'pilot'; pid: string; hash: string; ok: boolean };
// A developer's byte tunnel to the dongle's SSH server (meowdbg over the relay). The dongle
// opens it only while Settings > Developer > Network debugging is on, and SSH authenticates end
// to end; the relay just passes bytes. Binary frames to/from the device carry a 4-byte channel id.
type DevAttachment = { role: 'dev'; cid: number; hash: string; generation: number; ready: boolean; opened: number; active: number; window: number; bytes: number };
const MAX_TUNNELS = 2;
const MAX_TUNNEL_BYTES = 16_384;
const TUNNEL_BYTES_PER_SECOND = 1_048_576;
type DebugPolicy = { until: number; hashes: string[]; generation: number };
// The device's last status lives in its socket attachment: in-memory fields are lost whenever
// the object hibernates between messages.
type DeviceAttachment = { role: 'device'; since: number; dongle: boolean; debug?: DebugPolicy };
type PairResult = { ok: boolean; token?: string; error?: string };
type Info = { online: boolean; dongle: boolean };

async function sha256hex(s: string): Promise<string> {
  const d = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(s));
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

function randomId(): string {
  const b = crypto.getRandomValues(new Uint8Array(8));
  return [...b].map((x) => x.toString(16).padStart(2, '0')).join('');
}

function cors(req: Request, env: Env): Record<string, string> {
  const origin = req.headers.get('Origin') ?? '';
  const allowed = env.PILOT_ORIGINS.split(',').map((s) => s.trim());
  if (!allowed.includes(origin)) return {};
  return {
    'Access-Control-Allow-Origin': origin,
    'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
    'Access-Control-Allow-Headers': 'Content-Type',
    'Access-Control-Max-Age': '86400',
    Vary: 'Origin',
  };
}

function json(body: unknown, status: number, headers: Record<string, string>): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json', 'Cache-Control': 'no-store', ...headers },
  });
}

export class Device extends DurableObject<Env> {
  private pending = new Map<string, (r: PairResult) => void>();

  // The newest open device socket: after a reconnect the replaced socket can still be listed
  // for a while, and messages sent to it are lost.
  private device(): WebSocket | undefined {
    let best: WebSocket | undefined;
    let since = -1;
    for (const w of this.ctx.getWebSockets('device')) {
      if (w.readyState !== WebSocket.OPEN) continue;
      const a = w.deserializeAttachment() as DeviceAttachment | null;
      if (a && a.since > since) {
        since = a.since;
        best = w;
      }
    }
    return best;
  }

  private dongleUp(): boolean {
    const d = this.device();
    return d ? (d.deserializeAttachment() as DeviceAttachment | null)?.dongle === true : false;
  }

  private pilots(): WebSocket[] {
    return this.ctx.getWebSockets('pilot');
  }

  private send(ws: WebSocket | undefined, msg: unknown) {
    if (!ws) return false;
    try {
      ws.send(JSON.stringify(msg));
      return true;
    } catch {
      return false;
    }
  }

  private pilot(pid: unknown): WebSocket | undefined {
    return this.pilots().find((s) => (s.deserializeAttachment() as PilotAttachment | null)?.pid === pid);
  }

  private broadcast(msg: unknown) {
    for (const p of this.pilots()) {
      const a = p.deserializeAttachment() as PilotAttachment | null;
      if (a?.ok) this.send(p, msg);
    }
  }

  // WebSocket upgrades come through fetch() (a 101 response with a socket cannot cross RPC).
  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    if (url.pathname.startsWith('/device/')) return this.connectDevice(req.headers.get('X-Device-Key') ?? '');
    if (url.pathname.startsWith('/dev/')) return this.connectDev(req);
    return this.connectPilot(url.searchParams.get('k') ?? '');
  }

  private async connectDevice(key: string): Promise<Response> {
    const hash = await sha256hex(key);
    const stored = await this.ctx.storage.get<string>('key_hash');
    if (stored === undefined) {
      await this.ctx.storage.put('key_hash', hash);
    } else if (stored !== hash) {
      return new Response('wrong device key', { status: 403 });
    }
    for (const tunnel of this.ctx.getWebSockets('dev')) this.closeTunnel(tunnel, 4404, 'device reconnecting');
    for (const old of this.ctx.getWebSockets('device')) old.close(4000, 'replaced');
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, ['device']);
    server.serializeAttachment({ role: 'device', since: Date.now(), dongle: false } satisfies DeviceAttachment);
    // Pilots waiting for the device: ask it to check their tokens now.
    for (const p of this.pilots()) {
      const a = p.deserializeAttachment() as PilotAttachment;
      this.send(server, { t: 'hello', pid: a.pid, hash: a.hash });
    }
    return new Response(null, { status: 101, webSocket: client });
  }

  private debugPolicy(): DebugPolicy | undefined {
    const p = this.device()?.deserializeAttachment() as DeviceAttachment | undefined;
    return p?.debug && p.debug.until > Date.now() ? p.debug : undefined;
  }

  private async connectDev(req: Request): Promise<Response> {
    if (!this.device()) return new Response('device offline', { status: 503 });
    // No browser-based developer clients. A shared device ID grants no admission.
    if (req.headers.has('Origin')) return new Response('browser debugging is not supported', { status: 403 });
    const token = req.headers.get('Authorization')?.match(/^Bearer ([0-9a-f]{64})$/)?.[1];
    if (!token) return new Response('developer credential required', { status: 401 });
    const hash = await sha256hex(token);
    const dev = this.device();
    if (!dev) return new Response('device offline', { status: 503 });
    const policy = this.debugPolicy();
    if (!policy || !policy.hashes.includes(hash)) return new Response('debugging disabled or credential revoked', { status: 403 });
    if (this.ctx.getWebSockets('dev').filter((w) => w.readyState === WebSocket.OPEN).length >= MAX_TUNNELS) return new Response('too many tunnels', { status: 429 });
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, ['dev']);
    let cid: number;
    do { cid = crypto.getRandomValues(new Uint32Array(1))[0]; } while (this.tunnel(cid));
    const now = Date.now();
    server.serializeAttachment({ role: 'dev', cid, hash, generation: policy.generation, ready: false, opened: now, active: now, window: now, bytes: 0 } satisfies DevAttachment);
    this.send(dev, { t: 'tunnel-open', cid, hash });
    await this.ctx.storage.setAlarm(now + 5000);
    return new Response(null, { status: 101, webSocket: client });
  }

  private tunnel(cid: number): WebSocket | undefined {
    return this.ctx.getWebSockets('dev').find((w) => (w.deserializeAttachment() as DevAttachment | null)?.cid === cid);
  }

  private closeTunnel(ws: WebSocket, code: number, reason: string) {
    if (ws.readyState !== WebSocket.OPEN) return;
    const a = ws.deserializeAttachment() as DevAttachment;
    this.send(this.device(), { t: 'tunnel-close', cid: a.cid });
    this.send(ws, { t: 'closed', code, reason });
    ws.close(code, reason);
  }

  private admitted(a: DevAttachment): boolean {
    const p = this.debugPolicy();
    return !!p && p.generation === a.generation && p.hashes.includes(a.hash);
  }

  async alarm() {
    const now = Date.now();
    for (const ws of this.ctx.getWebSockets('dev')) {
      const a = ws.deserializeAttachment() as DevAttachment;
      if (!this.admitted(a) || (!a.ready && now - a.opened > 10_000) || now - a.active > 120_000) {
        this.closeTunnel(ws, 4403, 'debug session expired');
      }
    }
    if (this.ctx.getWebSockets('dev').some((w) => w.readyState === WebSocket.OPEN)) await this.ctx.storage.setAlarm(now + 5000);
  }

  private async connectPilot(token: string): Promise<Response> {
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, ['pilot']);
    const a: PilotAttachment = { role: 'pilot', pid: randomId(), hash: await sha256hex(token), ok: false };
    server.serializeAttachment(a);
    const dev = this.device();
    if (dev) this.send(dev, { t: 'hello', pid: a.pid, hash: a.hash });
    else this.send(server, { t: 'status', dongle: false, online: false });
    return new Response(null, { status: 101, webSocket: client });
  }

  async pair(code: string, name: string): Promise<PairResult> {
    const dev = this.device();
    if (!dev) return { ok: false, error: 'offline' };
    const rid = randomId();
    const result = new Promise<PairResult>((resolve) => {
      this.pending.set(rid, resolve);
      setTimeout(() => {
        if (this.pending.delete(rid)) resolve({ ok: false, error: 'timeout' });
      }, PAIR_TIMEOUT_MS);
    });
    this.send(dev, { t: 'pair', rid, code, name });
    return result;
  }

  async info(): Promise<Info> {
    return { online: !!this.device(), dongle: this.dongleUp() };
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    const role = (ws.deserializeAttachment() as { role?: string } | null)?.role;
    if (typeof message !== 'string') {
      // Tunnel bytes: dev -> device gets the channel id prepended; device -> dev has it stripped.
      if (role === 'dev') {
        const a = ws.deserializeAttachment() as DevAttachment;
        if (!a.ready || !this.admitted(a)) { this.closeTunnel(ws, 4403, 'debug session not authorised'); return; }
        if (message.byteLength > MAX_TUNNEL_BYTES) { this.closeTunnel(ws, 1009, 'debug frame too large'); return; }
        const now = Date.now();
        if (now - a.window >= 1000) { a.window = now; a.bytes = 0; }
        a.bytes += message.byteLength;
        if (a.bytes > TUNNEL_BYTES_PER_SECOND) { this.closeTunnel(ws, 4429, 'debug rate limit'); return; }
        a.active = now;
        ws.serializeAttachment(a);
        const out = new Uint8Array(4 + message.byteLength);
        new DataView(out.buffer).setUint32(0, a.cid);
        out.set(new Uint8Array(message), 4);
        this.device()?.send(out);
      } else if (role === 'device' && ws === this.device() && message.byteLength >= 4 && message.byteLength <= MAX_TUNNEL_BYTES + 4) {
        const cid = new DataView(message).getUint32(0);
        const tunnel = this.tunnel(cid);
        if (tunnel) {
          const a = tunnel.deserializeAttachment() as DevAttachment;
          if (a.ready && this.admitted(a)) {
            tunnel.serializeAttachment({ ...a, active: Date.now() });
            tunnel.send(message.slice(4));
          }
        }
      }
      return;
    }
    if (message.length > (role === 'device' ? MAX_DEVICE_MSG : MAX_MSG)) return;
    let m: Record<string, unknown>;
    try {
      m = JSON.parse(message);
    } catch {
      return;
    }
    if (!m || typeof m !== 'object' || Array.isArray(m)) return;
    const a = ws.deserializeAttachment() as PilotAttachment | DeviceAttachment | null;
    if (a?.role === 'device') {
      if (ws !== this.device()) return;
      switch (m.t) {
        case 'debug-policy': {
          const hashes = Array.isArray(m.hashes) ? m.hashes.filter((h): h is string => typeof h === 'string' && KEY_RE.test(h)).slice(0, 8) : [];
          const enabled = m.enabled === true && typeof m.generation === 'number' && Number.isSafeInteger(m.generation);
          const debug: DebugPolicy = { until: enabled ? Date.now() + Math.min(15_000, Math.max(0, Number(m.lease_ms) || 0)) : 0, hashes, generation: Number(m.generation) };
          ws.serializeAttachment({ ...a, debug });
          for (const tunnel of this.ctx.getWebSockets('dev')) {
            if (!this.admitted(tunnel.deserializeAttachment() as DevAttachment)) this.closeTunnel(tunnel, 4403, 'debug permission changed');
          }
          break;
        }
        case 'tunnel-ready': {
          const tunnel = this.tunnel(Number(m.cid));
          if (tunnel) {
            const ta = tunnel.deserializeAttachment() as DevAttachment;
            if (this.admitted(ta)) {
              tunnel.serializeAttachment({ ...ta, ready: true });
              this.send(tunnel, { t: 'ready' });
            } else this.closeTunnel(tunnel, 4403, 'debug permission changed');
          }
          break;
        }
        case 'auth': {
          const p = this.pilot(m.pid);
          if (m.ok !== true) console.log(JSON.stringify({ ev: 'pilot refused by device', pid: m.pid }));
          if (!p) break;
          const pa = p.deserializeAttachment() as PilotAttachment;
          if (m.ok === true) {
            p.serializeAttachment({ ...pa, ok: true });
            this.send(p, { t: 'status', dongle: this.dongleUp(), online: true });
          } else {
            // Also when the dongle withdraws an authorisation it gave ("Unpair phones"):
            // nothing more goes to or comes from this phone.
            p.serializeAttachment({ ...pa, ok: false });
            // Said in a message too: the close handshake can take a while to finish.
            this.send(p, { t: 'unpaired' });
            p.close(4401, 'bad token');
          }
          break;
        }
        // Heartbeat through this code (not only the runtime): the dongle reconnects when a
        // reply does not come, e.g. after a relay deploy left its socket half alive.
        case 'ping':
          this.send(ws, { t: 'pong' });
          break;
        case 'tunnel-close': {
          const t = this.tunnel(Number(m.cid));
          if (t) t.close(4403, String(m.reason ?? 'closed by the device'));
          break;
        }
        case 'status':
          ws.serializeAttachment({ ...(a as DeviceAttachment), dongle: m.dongle === true });
          this.broadcast({ t: 'status', dongle: m.dongle === true, online: true });
          break;
        // To one phone, or to all of them: whatever the dongle's pilot protocol says.
        case 'pilot': {
          if (!m.m || typeof m.m !== 'object' || Array.isArray(m.m)) break;
          if (typeof m.pid === 'string') {
            const p = this.pilot(m.pid);
            if (p && (p.deserializeAttachment() as PilotAttachment).ok) this.send(p, m.m);
          } else this.broadcast(m.m);
          break;
        }
        case 'pair-result': {
          const resolve = this.pending.get(String(m.rid));
          if (resolve) {
            this.pending.delete(String(m.rid));
            resolve(m.ok === true ? { ok: true, token: String(m.token) } : { ok: false, error: String(m.error ?? 'refused') });
          }
          break;
        }
      }
      return;
    }
    if (a?.role !== 'pilot' || !a.ok || typeof m.t !== 'string') return;
    if (m.t === 'key') {
      if (typeof m.k === 'string' && m.k.length <= 8) this.send(this.device(), { t: 'key', pid: a.pid, k: m.k });
      return;
    }
    // Wrapped, so a phone can never forge one of the relay's own messages to the dongle.
    this.send(this.device(), { t: 'msg', pid: a.pid, m });
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string) {
    const a = ws.deserializeAttachment() as PilotAttachment | DeviceAttachment | DevAttachment | null;
    if (a?.role === 'dev') this.send(this.device(), { t: 'tunnel-close', cid: a.cid });
    // The TV shows whether a phone is connected: tell the dongle when one leaves.
    if (a?.role === 'pilot' && a.ok) this.send(this.device(), { t: 'bye', pid: a.pid });
    if (a?.role === 'device' && !this.device()) {
      for (const t of this.ctx.getWebSockets('dev')) this.closeTunnel(t, 4404, 'device left');
      this.broadcast({ t: 'status', dongle: false, online: false });
    }
    // compatibility_date >= 2026-04-07: the runtime replies to Close frames.
  }

  async webSocketError(ws: WebSocket) {
    const a = ws.deserializeAttachment() as PilotAttachment | DeviceAttachment | DevAttachment | null;
    if (a?.role === 'pilot' && a.ok) this.send(this.device(), { t: 'bye', pid: a.pid });
  }
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    const h = cors(req, env);
    if (req.method === 'OPTIONS') return new Response(null, { status: 204, headers: h });
    const parts = url.pathname.split('/').filter(Boolean);

    // WebSockets: /device/<id> (the dongle) and /pilot/<id>?k=TOKEN (the phone).
    if ((parts[0] === 'device' || parts[0] === 'pilot' || parts[0] === 'dev') && parts.length === 2) {
      if (req.headers.get('Upgrade')?.toLowerCase() !== 'websocket') return new Response('expected websocket', { status: 426 });
      const id = parts[1];
      if (!ID_RE.test(id)) return new Response('bad device id', { status: 400 });
      const stub = env.DEVICE.getByName(id);
      if (parts[0] === 'dev') return stub.fetch(req);
      if (parts[0] === 'device') {
        const key = req.headers.get('X-Device-Key') ?? '';
        if (!KEY_RE.test(key)) return new Response('bad device key', { status: 400 });
        return stub.fetch(req);
      }
      const token = url.searchParams.get('k') ?? '';
      if (token.length < 16 || token.length > 64) return new Response('bad token', { status: 400 });
      return stub.fetch(req);
    }

    if (url.pathname === '/api/info' && req.method === 'GET') {
      const id = url.searchParams.get('device') ?? '';
      if (!ID_RE.test(id)) return json({ ok: false, error: 'bad_device' }, 400, h);
      return json({ ok: true, ...(await env.DEVICE.getByName(id).info()) }, 200, h);
    }

    if (url.pathname === '/api/pair' && req.method === 'POST') {
      let b: Record<string, unknown>;
      try {
        b = await req.json();
      } catch {
        return json({ ok: false, error: 'bad_request' }, 400, h);
      }
      const id = String(b.device ?? '');
      const code = String(b.code ?? '');
      if (!ID_RE.test(id) || !/^\d{6}$/.test(code)) return json({ ok: false, error: 'bad_request' }, 400, h);
      const r = await env.DEVICE.getByName(id).pair(code, String(b.name ?? 'phone').slice(0, 40));
      return json(r, r.ok ? 200 : r.error === 'offline' ? 503 : 403, h);
    }

    return json({ ok: false, error: 'not_found' }, 404, h);
  },
} satisfies ExportedHandler<Env>;
