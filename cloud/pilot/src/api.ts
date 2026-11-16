// The dongle's pilot API (apps/netd/src/http.rs). When the app is served by the Mac
// relay (server.mjs) instead, /api/info is not JSON and `getInfo` returns null.

export const TOKEN_KEY = 'dongle-remote-token';

// ---- Cloud mode (app on Cloudflare, dongle reached through the relay) --------------------
// VITE_RELAY is the relay's origin (https://HOST); cloud/deploy.sh sets it from config.env.
export const RELAY: string = import.meta.env.VITE_RELAY ?? '';
const DEVICE_KEY = 'meow-device';
const ID_RE = /^[0-9a-f]{32}$/;

/** The dongle this page talks to through the relay: `?d=` from the TV's QR code, else the
 * remembered one. Null when served by the dongle itself or by the Mac relay. */
export function cloudDevice(): string | null {
  const d = new URL(window.location.href).searchParams.get('d');
  if (d && ID_RE.test(d)) {
    try {
      localStorage.setItem(DEVICE_KEY, d);
    } catch {
      /* private mode */
    }
    return d;
  }
  if (window.location.hostname.endsWith('.pages.dev') || import.meta.env.VITE_CLOUD === '1') {
    try {
      const s = localStorage.getItem(DEVICE_KEY);
      return s && ID_RE.test(s) ? s : null;
    } catch {
      return null;
    }
  }
  return null;
}

export function cloudToken(device: string): string | null {
  try {
    return localStorage.getItem(`meow-pilot:${device}`);
  } catch {
    return null;
  }
}

export function saveCloudToken(device: string, token: string | null) {
  try {
    if (token) localStorage.setItem(`meow-pilot:${device}`, token);
    else localStorage.removeItem(`meow-pilot:${device}`);
  } catch {
    /* private mode */
  }
}

export async function cloudInfo(device: string): Promise<{ online: boolean; dongle: boolean } | null> {
  try {
    const r = await fetch(`${RELAY}/api/info?device=${device}`, { cache: 'no-store' });
    return await r.json();
  } catch {
    return null;
  }
}

export async function cloudPair(device: string, code: string): Promise<{ ok: true; token: string } | { ok: false; error: string }> {
  try {
    const r = await fetch(`${RELAY}/api/pair`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ device, code, name: deviceName() }),
    });
    return await r.json();
  } catch {
    return { ok: false, error: 'network' };
  }
}

export function cloudSocketUrl(device: string, token: string): string {
  return `${RELAY.replace(/^http/, 'ws')}/pilot/${device}?k=${encodeURIComponent(token)}`;
}

export type WifiState = 'none' | 'starting' | 'idle' | 'connecting' | 'dhcp' | 'connected' | 'failed';

export interface Info {
  name: string;
  version: string;
  via: 'setup' | 'lan';
  paired: boolean;
  pilots: number;
  ui: boolean;
  adapter: boolean;
  wifi: { state: WifiState; ssid: string; ip: string; signal: number; error: string; saved: string[] };
  setup: { active: boolean; ssid: string | null };
  pair_url?: string;
}

export interface Network {
  ssid: string;
  signal: number;
  secure: boolean;
}

export type JoinResult = { ok: true; ssid: string; ip: string; pair_url: string } | { ok: false; error: string };

export function loadToken(): string | null {
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function saveToken(t: string) {
  try {
    localStorage.setItem(TOKEN_KEY, t);
  } catch {
    /* private mode: the token lives only in this page */
  }
}

export function clearToken() {
  try {
    localStorage.removeItem(TOKEN_KEY);
  } catch {
    /* ignore */
  }
}

function headers(token: string | null): HeadersInit {
  const h: Record<string, string> = { 'Content-Type': 'application/json' };
  if (token) h['X-Pilot-Token'] = token;
  return h;
}

async function call<T>(path: string, token: string | null, init?: RequestInit, timeoutMs = 10000): Promise<T> {
  const ctl = new AbortController();
  const t = window.setTimeout(() => ctl.abort(), timeoutMs);
  try {
    const r = await fetch(path, { ...init, headers: headers(token), signal: ctl.signal, cache: 'no-store' });
    const text = await r.text();
    return JSON.parse(text) as T;
  } finally {
    window.clearTimeout(t);
  }
}

export async function getInfo(token: string | null): Promise<Info | null> {
  try {
    const i = await call<Info>('/api/info', token, undefined, 5000);
    return typeof i === 'object' && i && 'via' in i ? i : null;
  } catch {
    return null;
  }
}

export async function scan(token: string | null, fresh = false): Promise<Network[]> {
  const r = await call<{ ok: boolean; networks: Network[] }>(`/api/wifi/scan${fresh ? '?fresh=1' : ''}`, token, undefined, 12000);
  if (!r.ok) throw new Error('scan failed');
  return r.networks;
}

export function join(token: string | null, ssid: string, psk: string): Promise<JoinResult> {
  return call<JoinResult>('/api/wifi/join', token, { method: 'POST', body: JSON.stringify({ ssid, psk }) }, 90000);
}

export function forget(token: string | null): Promise<{ ok: boolean }> {
  return call('/api/wifi/forget', token, { method: 'POST', body: '{}' });
}

/** A short device label for the TV's list of paired phones. */
export function deviceName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return 'iPhone';
  if (/iPad/.test(ua)) return 'iPad';
  const m = ua.match(/Android[^;]*;\s*([^;)]+?)(?:\sBuild|\))/);
  if (m) return m[1].trim();
  if (/Macintosh/.test(ua)) return 'Mac';
  return 'phone';
}

export async function pair(code: string): Promise<{ ok: true; token: string } | { ok: false; error: string }> {
  try {
    return await call('/api/pair', null, { method: 'POST', body: JSON.stringify({ code, name: deviceName() }) });
  } catch {
    return { ok: false, error: 'network' };
  }
}

export const JOIN_ERRORS: Record<string, string> = {
  wrong_password: 'The password is not right.',
  not_found: 'The network is out of range.',
  no_ip: 'The network did not give the TV an address.',
  timeout: 'The TV could not connect in time.',
  busy: 'The TV is already connecting. Try again in a moment.',
  no_adapter: 'The Wi-Fi adapter is not ready.',
  bad_password: 'Wi-Fi passwords have 8 to 63 characters.',
  bad_ssid: 'That network name is not valid.',
  unauthorized: 'This phone is not allowed to change the Wi-Fi.',
};
