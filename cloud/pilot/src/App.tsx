import { useEffect, useRef, useState } from 'react';
import {
  AppWindow, ChevronDown, ChevronLeft, ChevronRight, ChevronUp, CornerUpLeft, Delete, FastForward, Gamepad2, Hand, House, Keyboard,
  LoaderCircle, Minus, Play, Plus, QrCode, Rewind, Send, Tv, Volume2, VolumeX, Wifi, X,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { cn } from '@/lib/utils';
import { readToken, useRemote, type Cmd, type Key, type Remote as RemoteState, type TvApp } from './useRemote';
import { clearToken, cloudDevice, cloudInfo, cloudPair, cloudSocketUrl, cloudToken, getInfo, pair, saveCloudToken, saveToken, type Info } from './api';
import { Setup } from './Setup';
import { Pair, Pairing, pairMessage } from './Pair';

const REPEAT_DELAY = 380;
const REPEAT_EVERY = 120;

/** Connection pill: what is wrong, or how good the link to the TV is (round-trip time). */
function Status({ conn, dongle, tvOnline, rtt, direct }: { conn: string; dongle: boolean; tvOnline: boolean | null; rtt: number | null; direct: boolean }) {
  const [netUp, setNetUp] = useState(() => navigator.onLine);
  useEffect(() => {
    const on = () => setNetUp(navigator.onLine);
    window.addEventListener('online', on);
    window.addEventListener('offline', on);
    return () => {
      window.removeEventListener('online', on);
      window.removeEventListener('offline', on);
    };
  }, []);
  const good = conn === 'online' && dongle;
  const [label, tone, busy] =
    conn === 'unauthorized'
      ? ['Pair again', 'bg-red-500', false]
      : !netUp
        ? ['Phone is offline', 'bg-red-500', false]
        : conn === 'connecting'
          ? [direct ? 'Connecting…' : 'Connecting to Mac…', 'bg-amber-400', true]
          : conn !== 'online'
            ? ['Reconnecting…', 'bg-red-500', true]
            : tvOnline === false
              ? ['TV is offline', 'bg-red-500', false]
              : !dongle
                ? [direct ? 'TV is starting…' : 'Waiting for dongle', 'bg-amber-400', false]
                : rtt !== null && rtt > 600
                  ? ['Slow connection', 'bg-amber-400', false]
                  : ['Connected', 'bg-emerald-400', false];
  // Signal bars from the round trip through the relay: 1 = slow, 3 = quick.
  const bars = !good || rtt === null ? 0 : rtt <= 180 ? 3 : rtt <= 600 ? 2 : 1;
  return (
    <div
      className="flex items-center gap-2 rounded-full border border-border bg-card/70 px-3 py-1.5 text-xs text-muted-foreground"
      role="status"
      title={good && rtt !== null ? `${rtt} ms to the TV and back` : undefined}
    >
      {busy ? (
        <LoaderCircle className="size-3 animate-spin text-amber-300" />
      ) : (
        <span className={cn('size-2 rounded-full', tone, good && 'shadow-[0_0_8px] shadow-emerald-400')} />
      )}
      {label}
      {bars > 0 && (
        <span className="flex items-end gap-px" aria-label={`${rtt} ms`}>
          {[1, 2, 3].map((i) => (
            <span key={i} className={cn('w-[3px] rounded-sm', i <= bars ? 'bg-emerald-400' : 'bg-border')} style={{ height: 3 + i * 3 }} />
          ))}
        </span>
      )}
    </div>
  );
}

/** Press-and-hold: `fire(false)` on press, then `fire(true)` repeatedly while held. */
function useHold() {
  const timer = useRef<number | undefined>(undefined);
  const stop = () => {
    window.clearTimeout(timer.current);
    window.clearInterval(timer.current);
    timer.current = undefined;
  };
  useEffect(() => stop, []);
  return (fire: (repeated: boolean) => void, repeat: boolean) => ({
    onPointerDown: (e: React.PointerEvent) => {
      e.preventDefault();
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      stop();
      fire(false);
      if (repeat) {
        timer.current = window.setTimeout(() => {
          timer.current = window.setInterval(() => fire(true), REPEAT_EVERY);
        }, REPEAT_DELAY);
      }
    },
    onPointerUp: stop,
    onPointerCancel: stop,
    onLostPointerCapture: stop,
    onContextMenu: (e: React.MouseEvent) => e.preventDefault(),
  });
}

/** The square that the D-pad or the touchpad fills: as large as the space allows. */
const PAD = 'relative aspect-square w-[min(100cqw,100cqh,19rem)]';

function DPad({ send }: { send: (k: Key) => boolean }) {
  const hold = useHold();
  const arm =
    'absolute flex items-center justify-center text-foreground/80 transition-colors active:text-primary active:bg-primary/10 rounded-2xl';
  const key = (k: Key, repeat: boolean) => hold(() => send(k), repeat);
  return (
    <div className={cn(PAD, 'rounded-full border border-border bg-gradient-to-b from-secondary/90 to-card shadow-[inset_0_1px_0_rgba(255,255,255,0.06),0_20px_50px_rgba(0,0,0,0.45)]')}>
      <button aria-label="Up" className={cn(arm, 'inset-x-[30%] top-[3%] h-[27%]')} {...key('up', true)}>
        <ChevronUp className="size-10" />
      </button>
      <button aria-label="Down" className={cn(arm, 'inset-x-[30%] bottom-[3%] h-[27%]')} {...key('down', true)}>
        <ChevronDown className="size-10" />
      </button>
      <button aria-label="Left" className={cn(arm, 'inset-y-[30%] left-[3%] w-[27%]')} {...key('left', true)}>
        <ChevronLeft className="size-10" />
      </button>
      <button aria-label="Right" className={cn(arm, 'inset-y-[30%] right-[3%] w-[27%]')} {...key('right', true)}>
        <ChevronRight className="size-10" />
      </button>
      <button
        aria-label="OK"
        className="absolute inset-[31%] flex items-center justify-center rounded-full border-2 border-primary/60 bg-background/80 text-lg font-semibold tracking-wide text-primary shadow-[0_0_24px_rgba(61,174,233,0.25)] transition active:scale-95 active:bg-primary active:text-primary-foreground"
        {...key('ok', false)}
      >
        OK
      </button>
    </div>
  );
}

/** Swipe pad: every ~56 px of travel moves one step (like a TV remote trackpad); tap = OK. */
function TouchPad({ send }: { send: (k: Key) => boolean }) {
  const origin = useRef<{ x: number; y: number; sx: number; sy: number; t: number; moved: boolean } | null>(null);
  const [pulse, setPulse] = useState<Key | null>(null);
  const STEP = 56;
  const fire = (k: Key) => {
    send(k);
    setPulse(k);
    window.setTimeout(() => setPulse(null), 140);
  };
  return (
    <div
      className={cn(PAD, 'flex touch-none items-center justify-center rounded-[2rem] border border-border bg-gradient-to-b from-secondary/70 to-card text-sm text-muted-foreground shadow-[0_20px_50px_rgba(0,0,0,0.45)]')}
      onPointerDown={(e) => {
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        origin.current = { x: e.clientX, y: e.clientY, sx: e.clientX, sy: e.clientY, t: performance.now(), moved: false };
      }}
      onPointerMove={(e) => {
        const o = origin.current;
        if (!o) return;
        const dx = e.clientX - o.x;
        const dy = e.clientY - o.y;
        if (Math.abs(dx) >= STEP && Math.abs(dx) >= Math.abs(dy)) {
          fire(dx > 0 ? 'right' : 'left');
          o.x += Math.sign(dx) * STEP;
          o.y = e.clientY;
          o.moved = true;
        } else if (Math.abs(dy) >= STEP) {
          fire(dy > 0 ? 'down' : 'up');
          o.y += Math.sign(dy) * STEP;
          o.x = e.clientX;
          o.moved = true;
        }
      }}
      onPointerUp={(e) => {
        const o = origin.current;
        origin.current = null;
        if (o && !o.moved && Math.hypot(e.clientX - o.sx, e.clientY - o.sy) < 12 && performance.now() - o.t < 350) fire('ok');
      }}
      onPointerCancel={() => (origin.current = null)}
    >
      <span className={cn('transition-colors', pulse && 'text-primary')}>
        {pulse ? pulse.toUpperCase() : 'Swipe to move · tap for OK'}
      </span>
    </div>
  );
}

/** The system volume: mute, step down/up (hold to keep going) and a bar to drag. The number
 * is the TV's own (it follows changes made on the TV or by an app). */
function Volume({ vol, muted, canMute, cmd }: { vol: number | null; muted: boolean; canMute: boolean; cmd: (c: Cmd) => boolean }) {
  const hold = useHold();
  const bar = useRef<HTMLDivElement>(null);
  const drag = useRef<{ last: number; sent: number } | null>(null);
  const off = vol === null;
  const shown = vol ?? 0;
  const setFrom = (e: React.PointerEvent, final: boolean) => {
    const d = drag.current;
    const r = bar.current?.getBoundingClientRect();
    if (!d || !r) return;
    const v = Math.max(0, Math.min(100, Math.round(((e.clientX - r.left) / r.width) * 20) * 5));
    const now = performance.now();
    // While dragging: at most ten changes a second (each one shows on the TV).
    if (v !== d.last && (final || now - d.sent > 100)) {
      d.last = v;
      d.sent = now;
      cmd({ t: 'vol', set: v });
    }
  };
  const btn = 'flex size-11 shrink-0 items-center justify-center rounded-xl text-foreground/90 transition-colors active:bg-primary/15 active:text-primary disabled:opacity-40';
  return (
    <div className="flex items-center gap-1 rounded-2xl border border-border bg-card/70 p-1" aria-label="Volume">
      {canMute && (
        <button className={cn(btn, muted && 'text-amber-300')} aria-label={muted ? 'Unmute' : 'Mute'} aria-pressed={muted} disabled={off} onClick={() => cmd({ t: 'mute' })}>
          {muted ? <VolumeX className="size-5" /> : <Volume2 className="size-5" />}
        </button>
      )}
      <button className={btn} aria-label="Volume down" disabled={off} {...hold((again) => cmd({ t: 'vol', d: again ? -2 : -5 }), true)}>
        <Minus className="size-5" />
      </button>
      <div
        ref={bar}
        role="slider"
        aria-label="Volume"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={shown}
        className={cn('relative h-11 min-w-0 flex-1 touch-none overflow-hidden rounded-xl bg-secondary/70', off && 'opacity-40')}
        onPointerDown={(e) => {
          if (off) return;
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
          drag.current = { last: -1, sent: 0 };
          setFrom(e, false);
        }}
        onPointerMove={(e) => setFrom(e, false)}
        onPointerUp={(e) => {
          setFrom(e, true);
          drag.current = null;
        }}
        onPointerCancel={() => (drag.current = null)}
      >
        <div className={cn('absolute inset-y-0 left-0 rounded-xl transition-[width] duration-100', muted ? 'bg-muted-foreground/40' : 'bg-primary/70')} style={{ width: `${shown}%` }} />
        <span className="absolute inset-0 flex items-center justify-center text-sm font-medium tabular-nums">
          {off ? '–' : muted ? 'Muted' : shown}
        </span>
      </div>
      <button className={btn} aria-label="Volume up" disabled={off} {...hold((again) => cmd({ t: 'vol', d: again ? 2 : 5 }), true)}>
        <Plus className="size-5" />
      </button>
    </div>
  );
}

/** A panel that slides over the lower part of the remote. */
function Sheet({ title, onClose, children }: { title: string; onClose: () => void; children: React.ReactNode }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);
  return (
    <div className="fixed inset-0 z-20 flex flex-col justify-end bg-black/55" onClick={onClose}>
      <div
        className="mx-auto flex max-h-[80%] w-full max-w-md flex-col rounded-t-3xl border border-b-0 border-border bg-card px-4 pb-[max(1rem,env(safe-area-inset-bottom))] pt-3 shadow-2xl"
        role="dialog"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-base font-medium">{title}</h2>
          <Button variant="ghost" size="icon" aria-label="Close" onClick={onClose}>
            <X className="size-5" />
          </Button>
        </div>
        {children}
      </div>
    </div>
  );
}

/** The apps installed on the TV: tap to open one there. */
function AppsSheet({ apps, fg, onLaunch, onHome, onClose }: { apps: TvApp[]; fg: string | null; onLaunch: (a: TvApp) => void; onHome: () => void; onClose: () => void }) {
  return (
    <Sheet title="Apps on the TV" onClose={onClose}>
      <div className="-mx-1 flex flex-col gap-1 overflow-y-auto px-1 pb-1">
        <button className="flex items-center gap-3 rounded-2xl px-3 py-2.5 text-left active:bg-secondary/70" onClick={onHome}>
          <span className="flex size-10 items-center justify-center rounded-xl bg-secondary text-foreground/80">
            <House className="size-5" />
          </span>
          <span className="flex-1">Home screen</span>
          {!fg && <span className="text-xs text-primary">On screen</span>}
        </button>
        {apps.map((a) => (
          <button key={a.id} className="flex items-center gap-3 rounded-2xl px-3 py-2.5 text-left active:bg-secondary/70" onClick={() => onLaunch(a)}>
            <span className="flex size-10 items-center justify-center rounded-xl bg-primary/15 text-lg font-semibold text-primary">
              {a.name.trim().charAt(0).toUpperCase() || '?'}
            </span>
            <span className="min-w-0 flex-1 truncate">{a.name}</span>
            {a.id === fg ? <span className="text-xs text-primary">On screen</span> : a.st === 'running' && <span className="text-xs text-muted-foreground">Running</span>}
          </button>
        ))}
        {apps.length === 0 && <p className="px-3 py-4 text-sm text-muted-foreground">No apps are installed yet. Install one from the TV&apos;s home screen.</p>}
      </div>
    </Sheet>
  );
}

/** Type on the phone, send to the app on the TV (a search or a login field there). */
function KeyboardSheet({ app, send, sendText, onClose }: { app: string | null; send: (k: Key) => boolean; sendText: (s: string) => Promise<boolean>; onClose: () => void }) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const hold = useHold();
  const submit = async () => {
    if (!text || busy) return;
    setBusy(true);
    setError('');
    const ok = await sendText(text);
    setBusy(false);
    if (ok) setText('');
    else setError(app ? `${app} did not take the text.` : 'No app on the TV is waiting for text.');
  };
  return (
    <Sheet title={app ? `Type in ${app}` : 'Type on the TV'} onClose={onClose}>
      <form
        className="flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <div className="flex gap-2">
          <input
            className="h-12 min-w-0 flex-1 select-text rounded-xl border border-border bg-background/70 px-3 text-base text-foreground outline-none focus:border-primary"
            value={text}
            maxLength={256}
            autoFocus
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
            enterKeyHint="send"
            placeholder="Text to send"
            aria-label="Text to send"
            onChange={(e) => setText(e.target.value)}
          />
          <Button type="submit" size="icon" className="size-12 rounded-xl" aria-label="Send" disabled={!text || busy}>
            {busy ? <LoaderCircle className="size-5 animate-spin" /> : <Send className="size-5" />}
          </Button>
        </div>
        {error && <p className="text-sm text-red-300">{error}</p>}
        <div className="grid grid-cols-2 gap-2">
          <Button type="button" variant="secondary" className="h-12 rounded-xl" {...hold(() => send('bksp'), true)}>
            <Delete className="size-5" /> Delete
          </Button>
          <Button type="button" variant="secondary" className="h-12 rounded-xl" {...hold(() => send('ok'), false)}>
            OK
          </Button>
        </div>
        <p className="text-xs text-muted-foreground">The text is added where the cursor is in the app on the TV. Delete removes the character before it.</p>
      </form>
    </Sheet>
  );
}

/** Keep the phone's screen on while the remote is open and visible. */
function useWakeLock() {
  useEffect(() => {
    let lock: WakeLockSentinel | null = null;
    let gone = false;
    const take = async () => {
      if (document.visibilityState !== 'visible' || lock) return;
      try {
        const l = await navigator.wakeLock?.request('screen');
        if (!l) return;
        if (gone) return void l.release();
        lock = l;
        // The browser drops the lock when the page is hidden: take it again when shown.
        l.addEventListener('release', () => {
          if (lock === l) lock = null;
        });
      } catch {
        /* low battery, or not allowed: the screen dims as usual */
      }
    };
    void take();
    document.addEventListener('visibilitychange', take);
    return () => {
      gone = true;
      document.removeEventListener('visibilitychange', take);
      void lock?.release();
    };
  }, []);
}

function LocalApp() {
  const [token, setToken] = useState<string | null>(readToken);
  // undefined: asking; null: not served by the dongle (Mac relay, server.mjs).
  const [info, setInfo] = useState<Info | null | undefined>(undefined);
  const [pairCode, setPairCode] = useState<string | null>(() => new URL(window.location.href).searchParams.get('pair'));
  const [pairError, setPairError] = useState<string | undefined>();
  const [wifiOpen, setWifiOpen] = useState(false);
  const r = useRemote(info === undefined || pairCode ? null : token);

  // Pairing link from the TV's QR code: http://<tv>/?pair=123456
  useEffect(() => {
    if (!pairCode) return;
    const url = new URL(window.location.href);
    url.searchParams.delete('pair');
    window.history.replaceState(null, '', url.toString());
    void pair(pairCode).then((r) => {
      if (r.ok) {
        saveToken(r.token);
        setToken(r.token);
      } else {
        setPairError(r.error === 'bad_code' ? 'That pairing link has expired. Enter the code shown on the TV.' : pairMessage(r.error));
      }
      setPairCode(null);
    });
  }, [pairCode]);

  // Dongle state (also tells whether this page is served by the dongle at all).
  useEffect(() => {
    let stop = false;
    const poll = async () => {
      const i = await getInfo(token);
      if (stop) return;
      setInfo((prev) => (i === null && prev ? prev : i));
      if (i && token && !i.paired && i.via === 'lan') {
        clearToken();
        setToken(null);
      }
    };
    void poll();
    const t = window.setInterval(poll, 8000);
    return () => {
      stop = true;
      window.clearInterval(t);
    };
  }, [token]);

  if (pairCode) return <Pairing />;
  if (info === undefined) {
    return (
      <main className="flex h-full items-center justify-center text-muted-foreground">
        <LoaderCircle className="size-8 animate-spin text-primary" />
      </main>
    );
  }
  // On the TV's setup network (opened as the captive portal): Wi-Fi setup only.
  if (info?.via === 'setup') return <Setup token={null} info={info} />;
  if (info && !token) {
    return (
      <Pair
        initialError={pairError}
        onPaired={(t) => {
          saveToken(t);
          setToken(t);
        }}
      />
    );
  }
  if (!token) {
    // The cloud app without a TV: it is opened from the TV's QR code, which names the TV.
    const cloud = import.meta.env.VITE_CLOUD === '1';
    return (
      <main className="flex h-full items-center justify-center p-6">
        <Card className="flex max-w-sm flex-col items-center gap-3 p-6 text-center">
          {cloud && <QrCode className="size-8 text-primary" />}
          <h1 className="text-lg font-semibold">MEOW Pilot</h1>
          {cloud ? (
            <p className="text-sm text-muted-foreground">
              Scan the QR code on your TV with this phone&apos;s camera to pair it: Settings › Wi-Fi › Pair a phone.
            </p>
          ) : (
            <p className="text-sm text-muted-foreground">
              Scan the QR code printed by <code className="text-foreground">npm run serve</code> on the Mac to pair this phone.
            </p>
          )}
        </Card>
      </main>
    );
  }
  if (wifiOpen && info) return <Setup token={token} info={info} onClose={() => setWifiOpen(false)} />;

  return <Remote r={r} dongle={r.dongle} direct={!!info || import.meta.env.VITE_CLOUD === '1'} onWifi={info?.adapter ? () => setWifiOpen(true) : undefined} />;
}

/** The remote itself: D-pad or touchpad, Back and Home, and, when the TV's firmware has
 * pilot protocol 2, the apps, text input, media keys and the volume. */
function Remote({ r, dongle, direct, onWifi }: { r: RemoteState; dongle: boolean; direct: boolean; onWifi?: () => void }) {
  const { conn, send, cmd, tv, apps } = r;
  const [mode, setMode] = useState<'dpad' | 'touch'>(() => {
    try {
      return localStorage.getItem('dongle-remote-mode') === 'touch' ? 'touch' : 'dpad';
    } catch {
      return 'dpad';
    }
  });
  useEffect(() => {
    try {
      localStorage.setItem('dongle-remote-mode', mode);
    } catch {
      /* private mode */
    }
  }, [mode]);
  const [sheet, setSheet] = useState<'apps' | 'keys' | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  useWakeLock();

  // What the TV can do right now (nothing extra with older firmware, or while it is away).
  // While the connection is away the controls stay where they are, dimmed.
  const live = conn === 'online' && r.tvOnline !== false && !!tv;
  const has = (c: string) => !!tv && tv.caps.includes(c);
  const away = !live && 'pointer-events-none opacity-40';
  const fg = tv?.fg ?? null;
  const fgName = fg ? (tv!.fgname ?? apps.find((a) => a.id === fg)?.name ?? 'An app') : null;

  // Hardware keyboard (desktop testing, or a BT keyboard on the phone).
  useEffect(() => {
    const map: Record<string, Key> = {
      ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right',
      Enter: 'ok', ' ': 'ok', Backspace: 'back', Escape: 'back', Home: 'home',
    };
    const onKey = (e: KeyboardEvent) => {
      if (sheet || (e.target as HTMLElement | null)?.tagName === 'INPUT') return;
      const k = map[e.key];
      if (k) {
        e.preventDefault();
        send(k);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [send, sheet]);

  // Messages from the TV (an app that could not be opened) and our own short confirmations.
  useEffect(() => {
    if (r.note) setToast(r.note.text);
  }, [r.note]);
  useEffect(() => {
    if (!toast) return;
    const t = window.setTimeout(() => setToast(null), 4000);
    return () => window.clearTimeout(t);
  }, [toast]);

  const hold = useHold();
  const key = (k: Key) => hold(() => send(k), false);
  const nav = 'h-12 flex-col gap-0.5 rounded-2xl px-1 text-[11px] font-normal';
  return (
    <main className="mx-auto flex h-full max-w-md flex-col gap-3 px-4 pb-[max(0.75rem,env(safe-area-inset-bottom))] pl-[max(1rem,env(safe-area-inset-left))] pr-[max(1rem,env(safe-area-inset-right))] pt-[max(0.75rem,env(safe-area-inset-top))]">
      <header className="flex items-center justify-between gap-2">
        <div className="min-w-0">
          <h1 className="text-base font-light leading-tight tracking-wide">MEOW Pilot</h1>
          {tv && (
            <p className={cn('flex items-center gap-1 truncate text-xs text-muted-foreground', !live && 'opacity-40')} aria-label="On the TV now">
              <Tv className="size-3 shrink-0" />
              <span className="truncate">{fgName ?? 'Home screen'}</span>
            </p>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Status conn={conn} dongle={dongle} tvOnline={r.tvOnline} rtt={r.rtt} direct={direct} />
          {onWifi && (
            <Button variant="ghost" size="icon" aria-label="Wi-Fi" onClick={onWifi}>
              <Wifi className="size-5" />
            </Button>
          )}
          <Button variant="ghost" size="icon" aria-label={mode === 'dpad' ? 'Switch to touchpad' : 'Switch to D-pad'} onClick={() => setMode(mode === 'dpad' ? 'touch' : 'dpad')}>
            {mode === 'dpad' ? <Hand className="size-5" /> : <Gamepad2 className="size-5" />}
          </Button>
        </div>
      </header>

      <section className={cn('flex min-h-0 flex-1 items-center justify-center transition-opacity [container-type:size]', conn !== 'online' && 'opacity-50')}>
        {mode === 'dpad' ? <DPad send={send} /> : <TouchPad send={send} />}
      </section>

      {has('media') && fg && (
        <div className={cn('grid grid-cols-3 gap-2', away)} aria-label="Playback">
          <Button variant="outline" className="h-11 rounded-2xl" aria-label="Back 10 seconds" {...hold(() => send('rew'), true)}>
            <Rewind className="size-5" />
          </Button>
          <Button variant="outline" className="h-11 rounded-2xl" aria-label="Play or pause" {...key('play')}>
            <Play className="size-5" />
            <span className="-ml-1 text-base leading-none">/</span>
            <span className="flex gap-[3px]" aria-hidden>
              <span className="h-4 w-[3px] rounded-sm bg-current" />
              <span className="h-4 w-[3px] rounded-sm bg-current" />
            </span>
          </Button>
          <Button variant="outline" className="h-11 rounded-2xl" aria-label="Forward 10 seconds" {...hold(() => send('fwd'), true)}>
            <FastForward className="size-5" />
          </Button>
        </div>
      )}

      <nav className={cn('grid gap-2', has('apps') || has('text') ? 'grid-cols-4' : 'grid-cols-2')}>
        <Button variant="secondary" className={nav} {...key('back')}>
          <CornerUpLeft className="size-5" /> Back
        </Button>
        <Button variant="secondary" className={nav} {...key('home')}>
          <House className="size-5" /> Home
        </Button>
        {(has('apps') || has('text')) && (
          <>
            <Button variant="secondary" className={nav} disabled={!live || !has('apps')} onClick={() => setSheet('apps')}>
              <AppWindow className="size-5" /> Apps
            </Button>
            <Button variant="secondary" className={nav} disabled={!live || !has('text') || !fg} onClick={() => setSheet('keys')}>
              <Keyboard className="size-5" /> Type
            </Button>
          </>
        )}
      </nav>

      {has('vol') && (
        <div className={cn('transition-opacity', away)}>
          <Volume vol={tv!.vol} muted={tv!.muted} canMute={has('mute')} cmd={cmd} />
        </div>
      )}

      {sheet === 'apps' && (
        <AppsSheet
          apps={apps}
          fg={fg}
          onClose={() => setSheet(null)}
          onHome={() => {
            send('home');
            setSheet(null);
          }}
          onLaunch={(a) => {
            setSheet(null);
            if (a.id === fg) return;
            if (cmd({ t: 'launch', id: a.id })) setToast(`Opening ${a.name} on the TV…`);
          }}
        />
      )}
      {sheet === 'keys' && <KeyboardSheet app={fgName} send={send} sendText={r.sendText} onClose={() => setSheet(null)} />}
      {toast && (
        <div className="pointer-events-none fixed inset-x-0 top-[max(3.75rem,calc(env(safe-area-inset-top)+3.25rem))] z-30 flex justify-center px-4" role="status">
          <div className="rounded-2xl border border-border bg-card px-4 py-2 text-sm shadow-xl">{toast}</div>
        </div>
      )}
    </main>
  );
}

/** Cloud mode: the app runs on Cloudflare Pages and reaches the dongle through the relay
 * (cloud/relay), so it works whatever network the phone is on. */
function CloudApp({ device }: { device: string }) {
  const [token, setToken] = useState<string | null>(() => cloudToken(device));
  const [pairCode, setPairCode] = useState<string | null>(() => new URL(window.location.href).searchParams.get('pair'));
  const [pairError, setPairError] = useState<string | undefined>();
  const [online, setOnline] = useState<boolean | null>(null);
  const r = useRemote(pairCode ? null : token, token && !pairCode ? cloudSocketUrl(device, token) : null);
  const { conn } = r;

  // No pairing code in the address bar or history. The TV's id stays (`?d=`): it is not a
  // secret, and it is what "Add to Home Screen" remembers. An installed web app on iOS has
  // its own storage, so it starts with the id only and asks for the TV's code once.
  useEffect(() => {
    const url = new URL(window.location.href);
    if (url.searchParams.get('d') !== device || url.searchParams.has('pair')) {
      url.searchParams.set('d', device);
      url.searchParams.delete('pair');
      window.history.replaceState(null, '', url.toString());
    }
  }, [device]);

  useEffect(() => {
    if (!pairCode) return;
    void cloudPair(device, pairCode).then((r) => {
      if (r.ok) {
        saveCloudToken(device, r.token);
        setToken(r.token);
      } else {
        setPairError(r.error === 'bad_code' ? 'That pairing link has expired. Enter the code shown on the TV.' : r.error === 'offline' ? 'The TV is not online. Check that it is on and connected to Wi-Fi.' : pairMessage(r.error));
      }
      setPairCode(null);
    });
  }, [pairCode, device]);

  useEffect(() => {
    let stop = false;
    const poll = async () => {
      const i = await cloudInfo(device);
      if (!stop && i) setOnline(i.online);
    };
    void poll();
    const t = window.setInterval(poll, 10000);
    return () => {
      stop = true;
      window.clearInterval(t);
    };
  }, [device]);

  // The TV no longer knows this phone (unpaired): pair again.
  useEffect(() => {
    if (conn === 'unauthorized') {
      saveCloudToken(device, null);
      setToken(null);
      setPairError('This phone is no longer paired. Enter the code shown on the TV.');
    }
  }, [conn, device]);

  if (pairCode) return <Pairing />;
  if (!token) {
    return (
      <Pair
        initialError={pairError ?? (online === false ? 'The TV is not online right now.' : undefined)}
        submit={(code) => cloudPair(device, code)}
        onPaired={(t) => {
          saveCloudToken(device, t);
          setToken(t);
        }}
      />
    );
  }
  return <Remote r={r} dongle={r.dongle && online !== false} direct />;
}

export default function App() {
  const [device] = useState(cloudDevice);
  return device ? <CloudApp device={device} /> : <LocalApp />;
}
