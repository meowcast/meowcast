#!/usr/bin/env node
// Dongle remote relay (runs on the Mac).
//
//   phone browser --WebSocket--> this server (Wi-Fi/LAN, :8080, token required)
//   this server  <--TCP-------- dongle (connects OUT from the USB side: /opt/ui/remote-link.sh)
//   dongle: bytes -> /run/ui-keys FIFO -> miracast-ui
//
// The dongle opens no listening port (a security rule of the firmware); the TCP listener here binds
// only to the Internet Sharing bridge (bridge100), the web server only to the LAN address.
//
// Usage: npm run serve   (build + start)  |  node server.mjs [--port 8080] [--lan en0]
//        [--usb bridge100] [--link-port 8002]

import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';
import qrcode from 'qrcode-terminal';

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (name, dflt) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
};
const ipv4 = (iface) =>
  os.networkInterfaces()[iface]?.find((a) => a.family === 'IPv4' && !a.internal)?.address;
const defaultIface = () => {
  try {
    return execSync('route -n get default', { encoding: 'utf8' }).match(/interface:\s*(\S+)/)?.[1];
  } catch {
    return undefined;
  }
};

const lanIface = arg('lan', defaultIface() || 'en0');
const usbIface = arg('usb', 'bridge100');
const webPort = Number(arg('port', 8080));
const linkPort = Number(arg('link-port', 8002));
const lanIp = ipv4(lanIface);
const usbIp = ipv4(usbIface);
if (!lanIp) {
  console.error(`No IPv4 address on ${lanIface}; pass --lan <iface>`);
  process.exit(1);
}

// Pairing token (persisted so a phone bookmark keeps working across restarts).
const tokenFile = path.join(here, '.token');
let token = process.env.REMOTE_TOKEN;
if (!token) {
  try {
    token = fs.readFileSync(tokenFile, 'utf8').trim();
  } catch {
    token = crypto.randomBytes(6).toString('base64url');
    fs.writeFileSync(tokenFile, token + '\n', { mode: 0o600 });
  }
}

// Keys understood by the UI's FIFO reader (ui/src/input.rs).
const KEYS = { up: 'w', down: 's', left: 'a', right: 'd', ok: 'e', back: 'b', home: 'm' };

// ---- dongle link -----------------------------------------------------------------------
let dongle = null;
const status = () => JSON.stringify({ t: 'status', dongle: !!dongle });
const broadcast = () => wss.clients.forEach((c) => c.readyState === 1 && c.send(status()));
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);

// allowHalfOpen: BusyBox nc half-closes when its stdin (/dev/null) hits EOF but keeps reading.
const link = net.createServer({ allowHalfOpen: true }, (sock) => {
  if (dongle) dongle.destroy();
  dongle = sock;
  sock.setNoDelay(true);
  sock.setKeepAlive(true, 5000);
  log(`dongle linked from ${sock.remoteAddress}`);
  broadcast();
  sock.on('data', () => {});
  sock.on('error', () => {});
  sock.on('close', () => {
    if (dongle === sock) {
      dongle = null;
      log('dongle link closed');
      broadcast();
    }
  });
});
if (usbIp) {
  link.listen(linkPort, usbIp, () => log(`dongle link listening on ${usbIp}:${linkPort} (${usbIface})`));
} else {
  log(`WARNING: no IPv4 on ${usbIface}: enable Internet Sharing to the dongle, then restart`);
}

// ---- web app ---------------------------------------------------------------------------
const dist = path.join(here, 'dist');
const MIME = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css',
  '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon', '.json': 'application/json',
  '.webmanifest': 'application/manifest+json', '.woff2': 'font/woff2',
};
const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://x');
  let file = path.resolve(dist, '.' + decodeURIComponent(url.pathname));
  if (!file.startsWith(dist + path.sep) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    file = path.join(dist, 'index.html'); // SPA fallback (also for "/")
  }
  if (!fs.existsSync(file)) {
    res.writeHead(500).end('dist/ missing: run "npm run build" first');
    return;
  }
  const immutable = file.includes(`${path.sep}assets${path.sep}`);
  res.writeHead(200, {
    'Content-Type': MIME[path.extname(file)] || 'application/octet-stream',
    'Cache-Control': immutable ? 'public, max-age=31536000, immutable' : 'no-cache',
  });
  fs.createReadStream(file).pipe(res);
});

const wss = new WebSocketServer({ noServer: true });
server.on('upgrade', (req, sock, head) => {
  const url = new URL(req.url, 'http://x');
  if (url.pathname !== '/ws') {
    sock.destroy();
    return;
  }
  wss.handleUpgrade(req, sock, head, (ws) => {
    if (url.searchParams.get('k') !== token) {
      ws.close(4401, 'bad token');
      return;
    }
    ws.isAlive = true;
    ws.on('pong', () => (ws.isAlive = true));
    ws.send(status());
    log(`phone connected (${req.socket.remoteAddress}), ${wss.clients.size} client(s)`);
    ws.on('message', (data) => {
      let m;
      try {
        m = JSON.parse(data);
      } catch {
        return;
      }
      if (m.t === 'key' && Object.hasOwn(KEYS, m.k)) {
        if (dongle) dongle.write(KEYS[m.k]);
        else ws.send(status());
      }
    });
  });
});
// Drop phones that went away without closing (sleep, Wi-Fi switch).
setInterval(() => {
  wss.clients.forEach((ws) => {
    if (!ws.isAlive) return ws.terminate();
    ws.isAlive = false;
    ws.ping();
  });
}, 15000);

server.listen(webPort, lanIp, () => {
  const url = `http://${lanIp}:${webPort}/?k=${token}`;
  console.log(`\nDongle remote: open on your phone (same Wi-Fi):\n  ${url}\n`);
  qrcode.generate(url, { small: true });
  console.log(
    `On the dongle (once per boot):\n  setsid /opt/ui/remote-link.sh >/dev/null 2>&1 </dev/null &\n`,
  );
});
