// Wi-Fi setup: pick the home network and send its password to the TV. Shown on the dongle's
// setup network (captive portal) and from the remote's Wi-Fi button.
import { useCallback, useEffect, useState } from 'react';
import { ArrowLeft, Check, Eye, EyeOff, LoaderCircle, Lock, RefreshCw, Tv, Wifi, WifiHigh, WifiLow, WifiZero } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { cn } from '@/lib/utils';
import { getInfo, join, JOIN_ERRORS, scan, type Info, type Network } from './api';

type Step =
  | { s: 'list' }
  | { s: 'password'; net: Network | null }
  | { s: 'joining'; ssid: string }
  | { s: 'done'; ssid: string; ip: string; pairUrl: string }
  | { s: 'error'; ssid: string; message: string; net: Network | null }
  | { s: 'lost'; ssid: string };

function Bars({ signal }: { signal: number }) {
  const I = signal >= -55 ? Wifi : signal >= -67 ? WifiHigh : signal >= -78 ? WifiLow : WifiZero;
  return <I className="size-5 text-foreground/80" />;
}

export function Setup({ token, info, onClose }: { token: string | null; info: Info | null; onClose?: () => void }) {
  const [step, setStep] = useState<Step>({ s: 'list' });
  const [nets, setNets] = useState<Network[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState(false);
  const [psk, setPsk] = useState('');
  const [ssid, setSsid] = useState('');
  const [show, setShow] = useState(false);

  const refresh = useCallback(
    async (fresh: boolean) => {
      setScanning(true);
      setScanError(false);
      try {
        setNets(await scan(token, fresh));
      } catch {
        setScanError(true);
      } finally {
        setScanning(false);
      }
    },
    [token],
  );
  useEffect(() => {
    void refresh(false);
  }, [refresh]);

  const connect = async (name: string, net: Network | null) => {
    setStep({ s: 'joining', ssid: name });
    try {
      const r = await join(token, name, psk);
      if (r.ok) setStep({ s: 'done', ssid: r.ssid, ip: r.ip, pairUrl: r.pair_url });
      else setStep({ s: 'error', ssid: name, message: JOIN_ERRORS[r.error] ?? 'The TV could not connect.', net });
    } catch {
      // The TV's setup network may move to the home network's channel while it connects, and
      // the phone can drop off for a moment: ask again until the answer is clear.
      for (let i = 0; i < 30; i++) {
        await new Promise((res) => window.setTimeout(res, 2000));
        const now = await getInfo(token);
        if (!now) continue;
        if (now.wifi.state === 'connected' && now.wifi.ssid === name) {
          setStep({ s: 'done', ssid: name, ip: now.wifi.ip, pairUrl: now.pair_url ?? '' });
          return;
        }
        if (now.wifi.error) {
          setStep({ s: 'error', ssid: name, message: now.wifi.error, net });
          return;
        }
      }
      setStep({ s: 'lost', ssid: name });
    }
  };

  const header = (title: string, back?: () => void) => (
    <header className="flex items-center gap-3 pb-4">
      {back && (
        <Button variant="ghost" size="icon" aria-label="Back" onClick={back}>
          <ArrowLeft className="size-5" />
        </Button>
      )}
      <h1 className="text-lg font-light tracking-wide">{title}</h1>
    </header>
  );

  const shell = (children: React.ReactNode) => (
    <main className="mx-auto flex h-full max-w-md flex-col overflow-y-auto px-5 pb-[max(1.25rem,env(safe-area-inset-bottom))] pt-[max(1rem,env(safe-area-inset-top))]">
      {children}
    </main>
  );

  if (step.s === 'list') {
    return shell(
      <>
        {header('Choose your Wi-Fi', onClose)}
        <p className="pb-4 text-sm text-muted-foreground">
          {info?.via === 'setup' ? `You are connected to ${info.setup.ssid ?? 'the TV'}. ` : ''}Pick the network the TV should use.
        </p>
        <Card className="divide-y divide-border overflow-hidden">
          {nets === null && !scanError && (
            <div className="flex items-center gap-3 p-4 text-sm text-muted-foreground">
              <LoaderCircle className="size-5 animate-spin" /> Looking for networks…
            </div>
          )}
          {scanError && <div className="p-4 text-sm text-red-300">The TV could not search for networks.</div>}
          {nets?.length === 0 && <div className="p-4 text-sm text-muted-foreground">No networks found.</div>}
          {nets?.map((n) => (
            <button
              key={n.ssid}
              className="flex w-full items-center gap-3 px-4 py-3.5 text-left active:bg-secondary/60"
              onClick={() => {
                setPsk('');
                setSsid(n.ssid);
                if (n.secure) setStep({ s: 'password', net: n });
                else void connect(n.ssid, n);
              }}
            >
              <Bars signal={n.signal} />
              <span className="flex-1 truncate">{n.ssid}</span>
              {n.secure && <Lock className="size-4 text-muted-foreground" />}
              {info?.wifi.ssid === n.ssid && info.wifi.state === 'connected' && <Check className="size-4 text-primary" />}
            </button>
          ))}
          <button
            className="flex w-full items-center gap-3 px-4 py-3.5 text-left text-muted-foreground active:bg-secondary/60"
            onClick={() => {
              setPsk('');
              setSsid('');
              setStep({ s: 'password', net: null });
            }}
          >
            <span className="size-5" />
            <span className="flex-1">Other network…</span>
          </button>
        </Card>
        <Button variant="outline" className="mt-4 self-center rounded-2xl" disabled={scanning} onClick={() => void refresh(true)}>
          <RefreshCw className={cn('size-4', scanning && 'animate-spin')} /> Search again
        </Button>
      </>,
    );
  }

  if (step.s === 'password') {
    const name = step.net?.ssid ?? ssid;
    const valid = name.length > 0 && (psk.length === 0 ? !step.net?.secure : psk.length >= 8 && psk.length <= 64);
    return shell(
      <>
        {header(step.net ? step.net.ssid : 'Other network', () => setStep({ s: 'list' }))}
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (valid) void connect(name, step.net);
          }}
        >
          {!step.net && (
            <label className="flex flex-col gap-1.5 text-sm text-muted-foreground">
              Network name
              <input
                className="h-12 rounded-xl border border-border bg-card px-4 text-base text-foreground outline-none focus:border-primary"
                value={ssid}
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                maxLength={32}
                onChange={(e) => setSsid(e.target.value)}
              />
            </label>
          )}
          <label className="flex flex-col gap-1.5 text-sm text-muted-foreground">
            Password{!step.net && ' (empty for an open network)'}
            <div className="relative">
              <input
                className="h-12 w-full select-text rounded-xl border border-border bg-card px-4 pr-12 text-base text-foreground outline-none focus:border-primary"
                type={show ? 'text' : 'password'}
                value={psk}
                autoFocus
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                autoComplete="current-password"
                maxLength={64}
                onChange={(e) => setPsk(e.target.value)}
              />
              <button type="button" aria-label={show ? 'Hide password' : 'Show password'} className="absolute inset-y-0 right-0 flex w-12 items-center justify-center text-muted-foreground" onClick={() => setShow(!show)}>
                {show ? <EyeOff className="size-5" /> : <Eye className="size-5" />}
              </button>
            </div>
          </label>
          <Button size="lg" className="rounded-2xl" disabled={!valid} type="submit">
            Connect
          </Button>
        </form>
      </>,
    );
  }

  if (step.s === 'joining') {
    return shell(
      <div className="flex flex-1 flex-col items-center justify-center gap-4 text-center">
        <LoaderCircle className="size-12 animate-spin text-primary" />
        <h1 className="text-lg font-light">Connecting the TV to {step.ssid}…</h1>
        <p className="max-w-xs text-sm text-muted-foreground">This takes up to half a minute. Keep this page open.</p>
      </div>,
    );
  }

  if (step.s === 'done') {
    const setupPhone = info?.via === 'setup';
    return shell(
      <div className="flex flex-1 flex-col items-center justify-center gap-4 text-center">
        <div className="flex size-16 items-center justify-center rounded-full bg-emerald-500/15">
          <Check className="size-9 text-emerald-400" />
        </div>
        <h1 className="text-xl font-light">The TV is on {step.ssid}</h1>
        {setupPhone ? (
          <>
            <Card className="flex items-center gap-3 p-4 text-left text-sm">
              <Tv className="size-6 shrink-0 text-primary" />
              <span>
                Now <b>scan the new code on the TV</b> with your phone's camera. It opens the pilot in your browser, where it stays paired.
              </span>
            </Card>
            <p className="max-w-xs text-sm text-muted-foreground">You can close this page. Your phone returns to its usual Wi-Fi by itself.</p>
          </>
        ) : (
          <>
            <p className="max-w-xs text-sm text-muted-foreground">Address {step.ip}. Move your phone to {step.ssid} too to keep using the pilot.</p>
            {onClose && (
              <Button size="lg" className="rounded-2xl" onClick={onClose}>
                Done
              </Button>
            )}
          </>
        )}
      </div>,
    );
  }

  if (step.s === 'error') {
    return shell(
      <div className="flex flex-1 flex-col items-center justify-center gap-4 text-center">
        <h1 className="text-xl font-light">Could not connect to {step.ssid}</h1>
        <p className="max-w-xs text-sm text-muted-foreground">{step.message}</p>
        <Button size="lg" className="rounded-2xl" onClick={() => setStep(step.net?.secure || !step.net ? { s: 'password', net: step.net } : { s: 'list' })}>
          Try again
        </Button>
        <Button variant="ghost" onClick={() => setStep({ s: 'list' })}>
          Choose another network
        </Button>
      </div>,
    );
  }

  return shell(
    <div className="flex flex-1 flex-col items-center justify-center gap-4 text-center">
      <h1 className="text-xl font-light">Your phone left the TV's network</h1>
      <p className="max-w-xs text-sm text-muted-foreground">
        If the TV now shows a new code, it is on {step.ssid}: scan the code with your camera. Otherwise join {info?.setup.ssid ?? 'the TV'}'s network again and retry.
      </p>
    </div>,
  );
}
