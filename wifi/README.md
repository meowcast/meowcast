# Wi-Fi, the pilot app and the network debug bridge

The dongle's original cable has a Wi-Fi adapter built in. Its USB-A end only carries power; the
micro-USB data lines go to the adapter. So with that cable the dongle has no USB link to a computer,
and its only way onto a network is Wi-Fi. This directory adds:

- **USB role switching.** The OTG port is a device (console + Ethernet to the Mac, as before) or the
  host of the Wi-Fi chip.
- **The RTL8733BU driver** in the kernel.
- **`wpa_supplicant`**, which runs the station, the setup access point and (for Miracast)
  the Wi-Fi Direct interface.
- **The pilot server** (`meow-netd`), with Wi-Fi setup and pairing from the phone.
- **The network debug bridge**: Dropbear SSH, switched on in Settings › Developer.

## The hardware

| Fact | Evidence |
|---|---|
| The Wi-Fi chip is a **Realtek RTL8733BU** (USB, 2.4 GHz 802.11n + BT 5) | The stock kernel builds exactly one Wi-Fi driver, `drivers/net/wireless/rockchip_wlan/rtl8733bu`. Its USB match table holds `0bda:b733` and `0bda:f72b` (class ff), and it loads `/system/etc/wifi/wifi_efuse_8733bu.map`. The stock DTB's `wifi_chip_type = "rtl8188eu"` is stale. |
| It sits on the **OTG port** in host mode | Stock DTB: `otg@30040000 { rockchip,usb-mode = <1>; }` is the vendor dwc_otg driver's "force host" setting. |
| No ID pin tells the two cables apart | So `rootfs/usb-role` decides by trying (below). |

Linux 7.3.4 has no driver for the 8733B (`rtw88`/`rtw89`/`rtl8xxxu` do not cover it). We build
Realtek's vendor driver in-tree. The source is
[Charliechen114514/rtl8733bu-linux-driver](https://github.com/Charliechen114514/rtl8733bu-linux-driver)
(branch `linux-7.1-port`, commit `2b72111`, GPL-2.0): the wirenboard tree `v5.15.12-264_for6.18`,
ported to 7.1 and verified on an RK3506B (also Cortex-A7). It is cloned in `vendor/rtl8733bu`
(git-ignored like every `vendor/`). The firmware is a C array inside the driver, so no
`/lib/firmware` files are needed.

## Files

| File | What |
|---|---|
| `kernel-wifi.sh` | Called by `../os/build.sh` when `WIFI=1`. Copies the driver into the kernel tree, wires Kconfig/Makefile and adds the config: dwc2 dual role, role switch, NETDEVICES, cfg80211, `RTL8733BU=y`. |
| `patch-driver.sh` | Our edits to the in-kernel copy, each asserted. Linux 7.3 API: `strncpy` removed, `WIPHY_FLAG_SUPPORTS_5_10_MHZ` gone, cfg80211 now passes the remain-on-channel/mgmt-tx cookie in. Size: `-Os`, log level warnings, no WoWLAN/monitor (that drops 250 KiB of WoWLAN firmware). `CONFIG_MP_INCLUDED` has to stay. |
| `build-userspace.sh` | Offline cross-build into `../os/deploy/wifi/bin`: `wpa_supplicant` + `wpa_cli` (2.12, static libnl 3.12, internal crypto, AP mode) and `dropbear` (2026.94, server only). Sources are pinned in `../os/fetch-sources.sh` (`WIFI=1`). |
| `wpa_supplicant.config`, `dropbear-localoptions.h` | Their build options. P2P, WPS and Wi-Fi Display are on in the supplicant's ([Miracast](#miracast-wi-fi-direct)). |
| `../apps/netd/src/p2p.rs` | Miracast's Wi-Fi Direct side. |
| `../os/rootfs/usb-role` | Chooses the port role at boot. |
| `../os/rootfs/netd-session` | Supervises `meow-netd`. |
| `../apps/netd/` | `meow-netd` (Rust). It is linked into the `appd` binary: `/usr/bin/meow-netd -> appd`. |
| `../ui/src/onboard.rs` | The TV's "Set up with your phone" prompt and the QR codes. |
| `../cloud/pilot/` | The pilot web app, served from Cloudflare (`npm run deploy:cloud`, see `../cloud/README.md`). The dongle keeps only `setup/index.html` (captive-portal Wi-Fi setup + a link to the pilot): `package-pilot.sh` stages it for `/usr/share/pilot`. |
| `../tools/meowdbg.py` | The Mac side of the debug bridge. |

## Build

```sh
WIFI=1 sh os/fetch-sources.sh                 # once: wpa_supplicant, libnl, dropbear
git clone https://github.com/Charliechen114514/rtl8733bu-linux-driver wifi/vendor/rtl8733bu
cd wifi && docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
  -w /fw/wifi meowcast-builder:gpu sh build-userspace.sh
cd ../cloud/pilot && sh package-pilot.sh                          # on the Mac (npm)
cd ../os && docker run --rm --network=none -e PROFILE=perf -e GPU_TEST=1 -v "$PWD/..:/fw" -w /fw/os \
  -v meowcast-build:/build meowcast-builder:gpu sh build.sh
```

`WIFI` defaults to 1 for `PROFILE=perf` when `deploy/wifi` exists and `/wifi` is mounted. The
initramfs is now **zstd -19** (`kernel.config`): 1.6 MB smaller than gzip. Without that the Wi-Fi
build would not fit in the kernel slot. The builder image got a `zstd` layer (`Dockerfile.gpu`).

Size, 14 Nov (zImage limit 11 728 896 bytes):

| | zImage |
|---|---|
| Before Wi-Fi (gzip initramfs) | 10 956 240 |
| With Wi-Fi (zstd initramfs) | 11 381 008 (348 KB headroom) |
| Latin font subsets, assets from the network store (14 Nov afternoon) | 10 973 888 (755 KB headroom) |

The new pieces: the driver and cfg80211 add about 1.3 MB to the kernel. `wpa_supplicant` is
716 KB (AP mode pulls in hostapd's authenticator); `meow-netd` adds 176 KB to `appd`. Dropbear is
105 KB, the pilot app 92 KB gzipped.

## How it works

**USB role** (`usb-role`, started by init after `/data` mounts). dwc2 runs as a dual-role controller
with a userspace role switch (DT: `dr_mode = "otg"; usb-role-switch;`, default peripheral):

1. It stays a device for 4 s. If a computer enumerates the gadget, it remains a device.
2. Otherwise it becomes a host (session-valid override; the board never drives VBUS).
3. If no USB device appears within 12 s, it goes back to device mode.

Override with `echo device|host > /data/net/usb-role`. The log is `/run/usb-role.log`, and every
boot is appended to `/data/net/usb-role.log`. On a host boot, a snapshot (dmesg, netd,
wpa_supplicant, state) is written to `/data/net/wifi-boot.log` after 90 s. Read it over the USB
console after a Wi-Fi-cable boot.

**Wi-Fi** (`meow-netd`):

- **Station.** `wpa_supplicant -i wlan0 -c /data/net/wpa.conf`; `udhcpc` once associated.
  WPA2-PSK and open networks only: WPA3-only (SAE) networks need an ECC library that is not
  built in.
- **When the setup network runs.** The driver's concurrent second interface (`wlan1`) runs a second
  `wpa_supplicant` in AP mode, `MEOW-<last 4 MAC hex digits>`. It starts when no network is
  saved, when the saved one has been unreachable for 45 s, or on request. While the station is
  connected, it uses the station's channel (one radio).
- **Setup network services.** WPA2 with an 8-digit password that changes every boot. `udhcpd`
  serves 192.168.73.10–60, and a captive DNS answers every name with 192.168.73.1. The HTTP server
  redirects other host names there, so the phone opens the pilot setup page as a captive portal
  ("Sign in to network").
- **When it closes.** Once the station is connected: 20 s after the last phone leaves, at most
  150 s.

**Pilot server** (port 80, all interfaces): the web app (gzip assets), `/api/*`, and `/ws`. The
WebSocket sends keys into the UI's FIFO (`/run/ui-keys`), the same byte protocol as `remote-link.sh`.

**Pilot protocol 2** (`apps/netd/src/pilot.rs`; the same JSON on `/ws` and, wrapped by the relay,
on the cloud link). The pilot shows only what the dongle lists in `caps`, so an older dongle or
pilot keeps working with keys alone.

| Message | Meaning |
|---|---|
| `{t:"key", k}` | `up down left right ok back home` → the launcher's FIFO. `play rew fwd bksp` → only the app on screen, as its keys `enter`, `left`, `right`, `backspace` |
| `{t:"vol", d}` / `{t:"vol", set}` | step or set the system volume |
| `{t:"mute"}` / `{t:"mute", on}` | toggle or set mute |
| `{t:"launch", id}` | open an installed app, closing the one on screen |
| `{t:"text", s}` → `{t:"text", ok}` | text (≤ 256 characters) for the app on screen: it arrives as the key `text:<s>` (`Key::Other` in the SDK) |
| `{t:"ping", n}` → `{t:"pong", n}` | heartbeat every 5 s; the pilot shows the round trip as signal bars |
| `{t:"state", v:2, caps, appd, vol, muted, fg, fgname, note, note_n}` | to the pilot: at connect and on every change |
| `{t:"apps", apps:[{id, name, st}]}` | to the pilot: installed apps (no system services), at most 24 |

netd gets all of this from appd as a `ctl` client of `/run/appd/ui.sock` (`apps/netd/src/appd.rs`),
like `appctl`: `VOLUME`, `VOLUME N|+N|-N`, `VOLUME mute on|off`, `PS` (the app with
`display=true` is on screen), `LIST`, `INFO id` (to leave out system services), `KEY name`,
`FOREGROUND id`. appd reports changes only to the launcher, so netd asks once a second, and only
while a pilot is connected. With an appd that has no mute, mute is volume 0 and unmute goes back
to the level before; with one that has no `FOREGROUND`, launching presses Home and then sends
`LAUNCH id`.

**Who is connected** (`/run/netd/state`, for the top bar's phone icon and Settings):

| Key | Meaning |
|---|---|
| `pilot_paired=0\|1` | at least one phone is paired (`pilots=N` is the count) |
| `pilot_connected=0\|1` | a paired phone is connected right now, on the LAN or through the relay (`online=N` is the count; `pilot_lan=N`, `pilot_cloud=N` split it) |

A phone stops counting when its socket closes (the relay passes that on as `bye`), 12 s after a
LAN phone stops answering WebSocket pings, 16 s after a relay phone's last heartbeat, and at once
when the cloud link drops or the phone is unpaired (it is closed with 4401).

| Endpoint | Who may call it |
|---|---|
| `GET /api/info` | anyone (`paired` says whether the token is valid) |
| `GET /api/wifi/scan`, `POST /api/wifi/join` | a phone on the setup network, or a paired pilot |
| `POST /api/wifi/forget` | a paired pilot |
| `POST /api/pair {code,name}` | anyone, only while the TV shows the code, see below |
| `GET /ws?k=TOKEN` | a paired pilot; close code 4401 otherwise |
| `POST /api/debug/pair/start`, `/finish` | SPAKE2, only during the TV enrollment window |

**Pairing.**

- The TV shows a 6-digit code: on the prompt (inside the QR link `http://<ip>/?pair=CODE`) and
  through Settings › Wi-Fi › Pair a phone.
- The phone exchanges it for a random 128-bit token in `localStorage`. The dongle keeps only
  SHA-256 hashes in `/data/net/pilots`.
- Pairing is accepted only while the TV shows the code (the UI sends `pair-open`/`pair-close`;
  the window expires after 15 min).
- Every success and every 5 wrong codes create a new code. Wrong codes also lock pairing for 30 s.
- Captive-portal browsers have throwaway storage, so the setup page does not pair. It asks the
  user to scan the TV's new code with the camera instead.

**The TV.**

- `ui/src/onboard.rs` reads `/run/netd/state` (key=value, QR codes as module matrices) every
  700 ms.
- It shows the prompt while an adapter is present and no pilot is paired, or while the dongle
  lost its Wi-Fi and runs the setup network. Back hides it until the phase changes.
- Settings › Wi-Fi shows status, signal, IP and the setup network, with *Set up with phone*,
  *Pair a phone*, *Unpair all phones* and *Forget this network*.
- Settings › Developer has the *Network debugging* toggle, with its address and code.
- Commands go to `/run/netd/ctl`: `setup | forget | scan | unpair | debug on|off | pair-code |
  pair-open | pair-close`.

**Debug bridge.**

- Network debugging is off after each boot and expires after one hour. USB access is
  available while it is on; LAN and cloud access have separate, initially-off switches.
- Choose **Pair developer computer** to show a six-digit code for two minutes. Run
  `meowdbg pair HOST` and enter the code when prompted. SPAKE2 authenticates enrollment
  and pins the same SSH host key for direct and cloud sessions.
- Select a listed developer computer to revoke its SSH key and cloud credential.
  Disabling debugging or revoking access terminates sessions through a dedicated cgroup.
- Pairing needs Python `spake2`; see [setup, protocol and migration](../cloud/DEBUG-SECURITY.md).
  Legacy developer keys need one secure re-pair after this update.

```sh
python3 tools/meowdbg.py pair 192.168.2.2    # once (creates ~/.ssh/meowdbg_ed25519)
python3 tools/meowdbg.py run 'cat /run/netd.log'
python3 tools/meowdbg.py push build/thing /opt/thing
python3 tools/meowdbg.py log wpa                       # ui | netd | appd | init | wpa | kernel
python3 tools/meowdbg.py shell
```

The initramfs `/` was mode 1777. init now makes it 755: Dropbear refuses keys under a
world-writable root, and nothing needed it writable.

## Miracast (Wi-Fi Direct)

**Not tested on hardware.** It was written on 14 Nov 2026 with the dongle on the USB lab link,
which has no Wi-Fi adapter. The logic is tested on the build host against event transcripts
(`cargo test -p meow-netd p2p`, `-p appd miracast`, `-p cast miracast`), and the supplicant
configurations it writes were parsed by a host build of the same wpa_supplicant. Miracast is
off unless the user switches it on in the Cast app; while it is off, `meow-netd` starts no
process for it and sends no P2P command.

**Who does what.** The Cast app (sandboxed) speaks RTSP and RTP. It asks appd for the
receiver (`wifi-direct` permission); appd writes `miracast on pbc|pin NAME`,
`miracast disconnect` or `miracast off` into `/run/netd/ctl`, repeats the request every 5 s
(netd drops a request that is 20 s old, so nothing stays up when appd or the app is gone)
and reads netd's state from `/run/netd/miracast`. `apps/netd/src/p2p.rs` does the rest.

**What the driver allows** (read from `vendor/rtl8733bu`, not measured):

| Fact | Where | Consequence |
|---|---|---|
| P2P only on the second interface | `rtw_sel_p2p_iface = IFACE_ID1`; `cfg80211_rtw_add_virtual_intf` refuses P2P types elsewhere | Wi-Fi Direct runs on `wlan1`, with its own `wpa_supplicant -i wlan1 -c /run/netd/p2p.conf` |
| No P2P device and no separate group interface | `RTW_P2P_GROUP_INTERFACE 0`; two interfaces at most (`rtw_combinations`) | `p2p_no_group_iface=1`: the group is formed on `wlan1` itself, and events name `wlan1` |
| `wlan1` is also the setup access point | one interface | **The setup network wins**: when it is needed, netd stops Wi-Fi Direct and the app shows "Unavailable: Wi-Fi setup in progress"; Miracast comes back when the setup network has closed |
| One channel for both interfaces | `num_different_channels = 1`, `CONFIG_CFG80211_ONECHANNEL_UNDER_CONCURRENT`, no MCC | **The station stays connected**, and the group has to be on its channel: wpa_supplicant offers only that channel in the negotiation. A source that cannot use it (it insists on 5 GHz; the chip is 2.4 GHz only) fails with `P2P-GO-NEG-FAILURE status=7` |
| Listening needs a social channel (1, 6, 11) | Wi-Fi Direct | The station's channel if it is one of them (continuous `P2P_LISTEN`, costs nothing); otherwise channel 6 for 450 ms of every second (`P2P_EXT_LISTEN 450 1000`), which takes that share of the station's air time while Miracast is on and idle |

**Sequence.**

1. `p2p.conf`: device name (the Cast name), device type 7-0050F204-1 (television),
   `config_methods=virtual_push_button` or `display` (PIN), `p2p_go_intent=1`, listen and
   operating channel, `persistent_reconnect=1`. Then `SET wifi_display 1`,
   `WFD_SUBELEM_SET 0 000600111c440032` (primary sink, session available, RTSP port 7236,
   50 Mbit/s), then listening.
2. A source asks: `P2P-PROV-DISC-PBC-REQ` or `P2P-GO-NEG-REQUEST ... dev_passwd_id=4` is
   answered with `P2P_CONNECT <peer> pbc go_intent=1`; in PIN mode
   `P2P-PROV-DISC-SHOW-PIN <peer> <pin>` puts the PIN on the TV (state `pin`) and is answered
   with `P2P_CONNECT <peer> <pin> display go_intent=1`. A method that is not the chosen one
   gets `P2P_REJECT`, and so does a second source while one is connected.
3. `P2P-GROUP-STARTED wlan1 client ...` (the usual case with intent 1): the address comes
   from the handshake (`ip_addr=`, `go_ip_addr=`) or from `udhcpc` with a script of our own
   that sets the address only (no default route, no DNS: those stay the station's). The
   source is the group owner or DHCP server.
   `... GO ...`: `wlan1` gets 192.168.49.1/24, `udhcpd` serves .10 to .20, and the source's
   address is its lease (by the interface address in `AP-STA-CONNECTED`).
4. State `peer <ip>:<port>` (the port from the source's WFD device information, `P2P_PEER`;
   7236 if it gives none): the app connects.
5. `P2P-GROUP-REMOVED`, the source leaving a group we own, or the app's `disconnect`: DHCP is
   stopped, addresses are flushed, listening starts again. Time-outs: 120 s for the
   negotiation (a PIN has to be typed), 30 s for the addresses.

**So that the station cannot be harmed for good:**

- The station's supplicant and DHCP client are never touched; it gets `p2p_disabled=1` (as
  does the setup access point, with `wps_disabled=1`) through a second configuration file,
  `-I /run/netd/no-p2p.conf`, so the saved networks file is not edited.
- Every way out (switched off, setup network, errors, a silent appd, netd exiting) goes
  through one `stop`: leave the group, `TERMINATE` the supplicant so that it puts `wlan1`
  back into station mode, kill DHCP, flush `wlan1`'s addresses.
- If the station was connected and stays disconnected for 20 s while Wi-Fi Direct is on,
  Wi-Fi Direct stops for 5 minutes ("the Wi-Fi connection was lost while it was on").
- A supplicant built without P2P is recognised by its binary
  (no `p2p_no_group_iface` string): then nothing above is passed to it and the app shows
  "Unavailable: this firmware has no Wi-Fi Direct support".
- Failures to start are shown as "Unavailable: ..." and tried again after 30 s.

**The supplicant build.** `CONFIG_P2P`, `CONFIG_WPS` and `CONFIG_WIFI_DISPLAY` are now set in
`wpa_supplicant.config`. Measured with the same toolchain and flags: 715 776 -> 1 088 536
bytes stripped (+373 KB), 359 KB -> 533 KB xz-compressed (+174 KB in the image). The binary in
`../os/deploy/wifi/bin` was **not** replaced: it is the one that was tested with the
station and the setup network, and the new one has never run on the chip. Run
`build-userspace.sh` (above) to put the P2P build into the next image. The kernel needs
nothing new (`CONFIG_CFG80211` and the driver's own `CONFIG_P2P`/`CONFIG_WFD` are in).

**First test on hardware** (Wi-Fi cable, home network joined, network debugging on; an image
with the P2P supplicant, or push it: `meowdbg push <wpa_supplicant> /opt/wpa_supplicant`, then
`PATH=/opt:$PATH netd-session restart`):

| Step | Look at |
|---|---|
| Before switching Miracast on | `meowdbg run 'cat /run/netd/miracast; ps \| grep wpa_'`: `off`, and one supplicant on `wlan0` with `-I /run/netd/no-p2p.conf`. `meowdbg log wpa` must not show P2P lines. The station connects as before |
| Cast app › Miracast › On | The row: Starting..., then "Ready — visible as NAME". `meowdbg log netd \| grep miracast`: `requested as`, `Wi-Fi Direct on wlan1 ... listen channel N, operating channel M`, `> SET wifi_display 1: OK`, `> WFD_SUBELEM_SET ...: OK`, `> P2P_EXT_LISTEN ...: OK`. `cat /run/netd/wpa-wlan1.log` for the supplicant's own messages (a `FAIL` on `SET wifi_display` means the driver did not report P2P on `wlan1`) |
| Station still fine? | `cat /run/netd/state` (`wifi=connected`), ping the dongle; the phone remote still works |
| Android: Settings › Cast (or "Smart View", "Wireless display"); Windows: Win+K | The TV's name appears. If not: `wpa_cli -p /run/wpa_supplicant -i wlan1 status` (`p2p_state=LISTEN_ONLY` or `IDLE`), and whether the station's channel is 1/6/11 |
| Tap the name | Log: `P2P-PROV-DISC-PBC-REQ` or `P2P-GO-NEG-REQUEST`, `> P2P_CONNECT ... pbc go_intent=1: OK`, `P2P-GO-NEG-SUCCESS`, `P2P-GROUP-STARTED wlan1 client ...`, then `joined the source's group; DHCP client on wlan1` (or the `GO` variant) and state `peer 192.168.49.x:7236`. `status=7` in `P2P-GO-NEG-FAILURE`: no common channel (see the table above) |
| Picture | `appctl logs dev.meow.cast` for the RTSP side (M1 to M7, then pictures) |
| Stop casting on the phone | `P2P-GROUP-REMOVED`, state `ready`, `ip addr show wlan1` without an address |
| Miracast › Off | State `off`, `Wi-Fi Direct stopped on wlan1`, only the station's supplicant left |
| PIN | Miracast PIN › On: the source asks for a PIN, the TV shows `PIN 1234 5678` |
| Setup network | Settings › Wi-Fi › Set up with phone while Miracast is on: the row says "Unavailable: Wi-Fi setup in progress", `MEOW-xxxx` appears; afterwards Miracast returns to Ready |

## Development over USB

- `meow-netd` runs on the USB link too, so the pilot, pairing and the debug bridge all work at
  `http://192.168.2.2/` from the Mac.
- With no Wi-Fi adapter the TV never opens the pairing window. Open it by hand:
  `echo pair-open > /run/netd/ctl`, then use `pair_code` from `/run/netd/state`.
- The Mac relay (`remote/server.mjs`) still works as before.

## Test status

Verified on the dongle over USB (RAM boot, then from flash, image `firmware-d7f811249d24`):

- **USB.** The dual-role kernel keeps the gadget; the Mac enumerates it in under 2 s and the role
  stays device.
- **Role switching.** Host mode registers the dwc2 root hub; back to device, the Mac re-enumerates.
- **Pilot server.**
  - The app and its gzip assets are served.
  - Pairing works: a wrong code gets 403, a closed window `not_open`, and 5 wrong codes rotate the
    code and lock pairing for 30 s.
  - The WebSocket delivers keys to the UI; a bad token closes with 4401.
- **Debug bridge.** Toggled from the TV in both directions; `meowdbg` pair, run, push and pull
  work; it comes back after a reboot.
- **TV.** Both prompt phases, Settings › Wi-Fi and Settings › Developer, checked on screenshots.
  The QR codes decode correctly from the framebuffer (CoreImage).

With the Wi-Fi cable (from flash, owner's phone and home router, 14 Nov):

1. **`firmware-d7f811249d24`.** Host mode failed (`dwc2_wait_for_mode: Couldn't set host mode`):
   the USB2 PHY's own OTG/charger state machine powered the PHY off. Fixed by
   `rockchip,controller-role` (kernel patch in `kernel-wifi.sh`, perf DTS).
2. **`firmware-c972cf872b62`.** It worked up to pairing:
   - the RTL8733BU enumerates and the driver creates `wlan0` and `wlan1`;
   - the setup network appears and the phone joins it by QR;
   - the captive setup page opens, and the dongle joins the home Wi-Fi (192.168.1.15);
   - the TV shows the pairing QR.

   The phone could not reach 192.168.1.15: the router isolates Wi-Fi clients (the Mac on the same
   Wi-Fi could not ping it either). That led to the cloud relay, see
   [../cloud/README.md](../cloud/README.md).
3. **`firmware-08a588b35534`** (cloud link, setup page only, the pilot moved to Cloudflare).
   Scanning the TV's QR code paired the phone through the relay, and the D-pad drives the TV.

Known limits:

- WPA3-only networks (no SAE).
- The debug bridge is reachable on USB or an explicitly enabled LAN without client isolation. It is
  available through the authenticated cloud bridge too; see [developer debugging](../cloud/DEBUG-SECURITY.md).
- `meow-netd` runs as root.
- Miracast's Wi-Fi Direct side is untested on hardware ([above](#miracast-wi-fi-direct)).
