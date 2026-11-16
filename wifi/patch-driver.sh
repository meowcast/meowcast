#!/bin/sh
# patch-driver.sh <driver dir>: local adjustments to the vendored RTL8733BU driver copy inside
# the kernel tree (the pristine vendor tree in vendor/rtl8733bu stays untouched). Each edit
# asserts that it applied, so a vendor update that moves the code fails the build loudly.
set -eu
python3 - "$1" <<'PY'
import re, sys
from pathlib import Path
d = Path(sys.argv[1])

def edit(rel, old, new, count=1):
    p = d / rel
    s = p.read_text(errors='surrogateescape')
    n = s.count(old)
    if n != count:
        sys.exit(f'patch-driver: {rel}: expected {count} x {old[:60]!r}, found {n}')
    p.write_text(s.replace(old, new), errors='surrogateescape')

# Linux 7.3 removed strncpy(). Same semantics (pad with NULs, no terminator when the source
# fills the buffer), local to the driver.
edit('include/osdep_service_linux.h', '#include <linux/version.h>\n', '''#include <linux/version.h>
#include <linux/string.h>
/* miracast: strncpy() is gone in Linux 7.3 */
static inline char *rtw_strncpy(char *dst, const char *src, size_t n)
{
	size_t i = 0;

	for (; i < n && src[i]; i++)
		dst[i] = src[i];
	for (; i < n; i++)
		dst[i] = '\\0';
	return dst;
}
#define strncpy rtw_strncpy
''')

f = 'os_dep/linux/ioctl_cfg80211.c'
# WIPHY_FLAG_SUPPORTS_5_10_MHZ is gone (5/10 MHz channels were removed from cfg80211).
edit(f, '\twiphy->flags |= WIPHY_FLAG_SUPPORTS_5_10_MHZ;\n',
     '#ifdef WIPHY_FLAG_SUPPORTS_5_10_MHZ\n\twiphy->flags |= WIPHY_FLAG_SUPPORTS_5_10_MHZ;\n#endif\n')
# Linux 7.3: cfg80211 allocates the remain-on-channel and mgmt-tx cookies and passes them in
# by value (remain_on_channel also gets the rx address). The driver keeps its pointer-based
# bodies; it must report the cookie it was given instead of generating its own.
edit(f, '\tunsigned int duration, u64 *cookie)\n{\n\ts32 err = 0;\n\tu8 remain_ch',
     '\tunsigned int duration, u64 cookie_in, const u8 *rx_addr)\n{\n\tu64 cookie_v = cookie_in, *cookie = &cookie_v;\n\ts32 err = 0;\n\tu8 remain_ch')
edit(f, '\t*cookie = ATOMIC_INC_RETURN(&prochinfo->ro_ch_cookie_gen);\n',
     '\t/* miracast: cookie comes from cfg80211 (Linux 7.3) */\n')
edit(f, '\tstruct cfg80211_mgmt_tx_params *params,\n#endif\n\tu64 *cookie)\n{\n',
     '\tstruct cfg80211_mgmt_tx_params *params,\n#endif\n\tu64 cookie_in)\n{\n\tu64 cookie_v = cookie_in, *cookie = &cookie_v;\n')
edit(f, '\t*cookie = pwdev_priv->mgmt_tx_cookie++;\n',
     '\t/* miracast: cookie comes from cfg80211 (Linux 7.3) */\n')

# Size: Realtek builds with -O1 and full debug logging. -Os, and only warnings/errors in the
# log (RTW_LOG_LEVEL 2 = _DRV_WARNING_); the debug proc files stay for diagnostics.
edit('Makefile', 'ccflags-y += -O1 ', 'ccflags-y += -Os ')
edit('Makefile', '-DRTW_LOG_LEVEL=4', '-DRTW_LOG_LEVEL=2')
# Not used on the dongle: Wake-on-WLAN (and its two 125 KiB firmware images), Wake-on-BT,
# monitor mode. (CONFIG_MP_INCLUDED stays: rtl8733b_ops.c needs it.)
for flag in ('-DCONFIG_WOWLAN ', '-DCONFIG_WAKE_ON_BT ', '-DCONFIG_WIFI_MONITOR '):
    edit('Makefile', flag, '')
PY
