// Pairing: the 6-digit code from the TV (the QR code carries it in the link; this screen is for
// typing it). The TV shows it on its "Set up with your phone" screen and in Settings > Wi-Fi.
import { useState } from 'react';
import { KeyRound, LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { pair } from './api';

export function pairMessage(error: string): string {
  switch (error) {
    case 'bad_code':
      return 'That code is not right. Check the TV and try again.';
    case 'not_open':
      return 'Open Settings › Wi-Fi › Pair a phone on the TV first.';
    case 'locked':
      return 'Too many tries. Wait half a minute, then use the new code on the TV.';
    default:
      return 'The TV did not answer.';
  }
}

export function Pair({
  onPaired,
  initialError,
  submit: doPair = pair,
}: {
  onPaired: (token: string) => void;
  initialError?: string;
  submit?: (code: string) => Promise<{ ok: true; token: string } | { ok: false; error: string }>;
}) {
  const [code, setCode] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(initialError ?? '');

  const submit = async () => {
    setBusy(true);
    setError('');
    const r = await doPair(code);
    setBusy(false);
    if (r.ok) onPaired(r.token);
    else setError(pairMessage(r.error));
  };

  return (
    <main className="flex h-full items-center justify-center p-6">
      <Card className="flex w-full max-w-sm flex-col gap-4 p-6 text-center">
        <KeyRound className="mx-auto size-8 text-primary" />
        <h1 className="text-lg font-semibold">Pair with your TV</h1>
        <p className="text-sm text-muted-foreground">Enter the 6-digit code shown on the TV (Settings › Wi-Fi › Pair a phone).</p>
        <form
          className="flex flex-col gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (code.length === 6 && !busy) void submit();
          }}
        >
          <input
            className="h-14 select-text rounded-xl border border-border bg-card text-center font-mono text-3xl tracking-[0.4em] text-foreground outline-none focus:border-primary"
            inputMode="numeric"
            autoComplete="one-time-code"
            pattern="[0-9]*"
            maxLength={6}
            value={code}
            autoFocus
            onChange={(e) => setCode(e.target.value.replace(/\D/g, '').slice(0, 6))}
          />
          {error && <p className="text-sm text-red-300">{error}</p>}
          <Button size="lg" className="rounded-2xl" type="submit" disabled={code.length !== 6 || busy}>
            {busy ? <LoaderCircle className="size-5 animate-spin" /> : 'Pair'}
          </Button>
        </form>
      </Card>
    </main>
  );
}

export function Pairing() {
  return (
    <main className="flex h-full flex-col items-center justify-center gap-3 p-6 text-muted-foreground">
      <LoaderCircle className="size-8 animate-spin text-primary" />
      Pairing with the TV…
    </main>
  );
}
