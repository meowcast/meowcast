import { useCallback, useEffect, useRef, useState } from 'react';
import { loadToken, saveToken } from './api';

/** Navigation keys go to the launcher (and through it to the app on screen). `play`, `rew`,
 * `fwd` and `bksp` go only to the app on screen (pilot protocol 2, capability `media`/`text`). */
export type Key = 'up' | 'down' | 'left' | 'right' | 'ok' | 'back' | 'home' | 'play' | 'rew' | 'fwd' | 'bksp';
export type Conn = 'connecting' | 'online' | 'offline' | 'unauthorized';

/** What the TV reports (pilot protocol 2, apps/netd/src/pilot.rs). Null with older
 * firmware: the remote then shows the keys only. */
export interface Tv {
  caps: string[];
  /** The app manager answers (volume and apps are known). */
  appd: boolean;
  vol: number | null;
  muted: boolean;
  /** The app on screen, if any. */
  fg: string | null;
  fgname: string | null;
}

export interface TvApp {
  id: string;
  name: string;
  st: string;
}

/** A command of pilot protocol 2. */
export type Cmd =
  | { t: 'vol'; d: number }
  | { t: 'vol'; set: number }
  | { t: 'mute'; on?: boolean }
  | { t: 'launch'; id: string }
  | { t: 'text'; s: string };

const PING_EVERY = 5000;
/** No answer to two pings: the connection is dead even if the socket has not noticed. */
const PONG_TIMEOUT = 11000;
/** A socket that opened but brought nothing (the relay is waiting for a TV that has stopped
 * answering): try again. */
const FIRST_MESSAGE_TIMEOUT = 8000;

/** Token from `?k=` (QR code printed by the Mac relay, server.mjs), remembered for later visits;
 * otherwise the one stored when this phone paired with the dongle. */
export function readToken(): string | null {
  const url = new URL(window.location.href);
  const k = url.searchParams.get('k');
  if (k) {
    saveToken(k);
    url.searchParams.delete('k');
    window.history.replaceState(null, '', url.toString());
    return k;
  }
  return loadToken();
}

function vibrate(ms: number) {
  try {
    navigator.vibrate?.(ms);
  } catch {
    /* not allowed before a user gesture */
  }
}

/** `url`: the WebSocket to use (cloud relay); default: /ws on this host with `token`. */
export function useRemote(token: string | null, url?: string | null) {
  const [conn, setConn] = useState<Conn>('connecting');
  const [dongle, setDongle] = useState(false);
  /** Cloud relay only: the dongle's link to the relay is up. */
  const [tvOnline, setTvOnline] = useState<boolean | null>(null);
  const [tv, setTv] = useState<Tv | null>(null);
  const [apps, setApps] = useState<TvApp[]>([]);
  /** Round trip phone -> TV -> phone in ms (null: not measured, or older firmware). */
  const [rtt, setRtt] = useState<number | null>(null);
  const [note, setNote] = useState<{ text: string; n: number } | null>(null);
  const ws = useRef<WebSocket | null>(null);
  const onText = useRef<((ok: boolean) => void) | null>(null);

  useEffect(() => {
    if (!token) return;
    let stopped = false;
    let retry = 500;
    let timer: number | undefined;
    let beat: number | undefined;
    let quiet: number | undefined;
    // Heartbeat state of the current socket.
    let v2 = false;
    let pingN = 0;
    let pingAt = 0;
    let lastPong = 0;
    let noteN: number | null = null;

    const reconnect = () => {
      window.clearInterval(beat);
      window.clearTimeout(quiet);
      setConn('offline');
      setDongle(false);
      setRtt(null);
      timer = window.setTimeout(connect, retry);
      retry = Math.min(retry * 2, 5000);
    };

    const connect = () => {
      const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
      const sock = new WebSocket(url ?? `${proto}://${window.location.host}/ws?k=${encodeURIComponent(token)}`);
      ws.current = sock;
      v2 = false;
      setConn((c) => (c === 'offline' ? 'offline' : 'connecting'));
      const drop = () => {
        sock.onclose = null;
        sock.close();
        if (ws.current === sock) ws.current = null;
        reconnect();
      };
      const ping = () => {
        if (sock.readyState !== WebSocket.OPEN || document.visibilityState !== 'visible') return;
        // A TV with protocol 2 answers every ping. Silence: drop the socket and start again
        // (a phone that changed networks keeps a socket that never errors).
        if (v2 && performance.now() - lastPong > PONG_TIMEOUT) return drop();
        pingAt = performance.now();
        sock.send(JSON.stringify({ t: 'ping', n: ++pingN }));
      };
      let first = true;
      sock.onopen = () => {
        lastPong = performance.now();
        // "Online" is the first message (the TV's status), not the open socket: the relay
        // accepts a phone before the TV has said that it knows it.
        quiet = window.setTimeout(drop, FIRST_MESSAGE_TIMEOUT);
        beat = window.setInterval(ping, PING_EVERY);
      };
      sock.onmessage = (ev) => {
        let m: Record<string, unknown>;
        try {
          m = JSON.parse(ev.data);
        } catch {
          return;
        }
        if (!m || typeof m !== 'object') return;
        if (first) {
          first = false;
          retry = 500;
          window.clearTimeout(quiet);
          setConn('online');
        }
        switch (m.t) {
          case 'status':
            setDongle(!!m.dongle);
            if (typeof m.online === 'boolean') {
              setTvOnline(m.online);
              // The TV left the relay: its last state is not worth showing.
              if (!m.online) v2 = false;
            }
            break;
          case 'state': {
            // The first state on this socket: measure the round trip now, not in 5 s.
            if (!v2) window.setTimeout(ping, 0);
            v2 = true;
            lastPong = performance.now();
            setTv({
              caps: Array.isArray(m.caps) ? m.caps.filter((c): c is string => typeof c === 'string') : [],
              appd: m.appd === true,
              vol: typeof m.vol === 'number' ? m.vol : null,
              muted: m.muted === true,
              fg: typeof m.fg === 'string' ? m.fg : null,
              fgname: typeof m.fgname === 'string' ? m.fgname : null,
            });
            // A note is news only if it arrived while this page was connected.
            const n = typeof m.note_n === 'number' ? m.note_n : 0;
            if (noteN !== null && n !== noteN && typeof m.note === 'string' && m.note) setNote({ text: m.note, n });
            noteN = n;
            break;
          }
          case 'apps':
            if (Array.isArray(m.apps)) setApps(m.apps.filter((a): a is TvApp => !!a && typeof a.id === 'string' && typeof a.name === 'string'));
            break;
          // The TV no longer knows this phone (the relay closes the socket right after).
          case 'unpaired':
            stopped = true;
            sock.onclose = null;
            sock.close();
            if (ws.current === sock) ws.current = null;
            window.clearInterval(beat);
            window.clearTimeout(quiet);
            setConn('unauthorized');
            break;
          case 'pong':
            lastPong = performance.now();
            if (m.n === pingN) setRtt(Math.round(lastPong - pingAt));
            break;
          case 'text':
            onText.current?.(m.ok === true);
            onText.current = null;
            break;
        }
      };
      sock.onclose = (ev) => {
        if (ws.current === sock) ws.current = null;
        window.clearInterval(beat);
        window.clearTimeout(quiet);
        if (stopped) return;
        if (ev.code === 4401) {
          setConn('unauthorized');
          return;
        }
        reconnect();
      };
    };
    connect();

    // Phones suspend sockets in the background: reconnect quickly when shown again, and do
    // not judge a socket by pings the page could not send while it was hidden.
    const onVisible = () => {
      if (document.visibilityState !== 'visible') return;
      lastPong = performance.now();
      if (!ws.current) {
        window.clearTimeout(timer);
        retry = 500;
        connect();
      }
    };
    const onOnline = () => {
      if (!ws.current) {
        window.clearTimeout(timer);
        retry = 500;
        connect();
      }
    };
    document.addEventListener('visibilitychange', onVisible);
    window.addEventListener('online', onOnline);
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      window.clearInterval(beat);
      window.clearTimeout(quiet);
      document.removeEventListener('visibilitychange', onVisible);
      window.removeEventListener('online', onOnline);
      ws.current?.close();
      ws.current = null;
      setTv(null);
      setApps([]);
      setRtt(null);
      setTvOnline(null);
    };
  }, [token, url]);

  const raw = useCallback((m: unknown, buzz = 8) => {
    const sock = ws.current;
    if (!sock || sock.readyState !== WebSocket.OPEN) return false;
    sock.send(JSON.stringify(m));
    vibrate(buzz);
    return true;
  }, []);

  const send = useCallback((k: Key) => raw({ t: 'key', k }), [raw]);

  const cmd = useCallback(
    (c: Cmd) => {
      // Show a volume change at once; the TV's state follows and corrects it.
      if (c.t === 'vol') {
        setTv((t) => {
          if (!t || t.vol === null) return t;
          const vol = Math.max(0, Math.min(100, 'set' in c ? c.set : t.vol + c.d));
          return { ...t, vol };
        });
      }
      return raw(c);
    },
    [raw],
  );

  /** Text for the app on screen. Resolves false when no app took it (or nothing answered). */
  const sendText = useCallback(
    (s: string) =>
      new Promise<boolean>((resolve) => {
        const done = (ok: boolean) => {
          window.clearTimeout(t);
          resolve(ok);
        };
        const t = window.setTimeout(() => {
          if (onText.current === done) onText.current = null;
          resolve(false);
        }, 4000);
        onText.current = done;
        if (!raw({ t: 'text', s })) done(false);
      }),
    [raw],
  );

  return { conn, dongle, tvOnline, tv, apps, rtt, note, send, cmd, sendText };
}

export type Remote = ReturnType<typeof useRemote>;
