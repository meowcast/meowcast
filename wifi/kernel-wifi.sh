#!/bin/sh
# kernel-wifi.sh <linux-src> <linux-out/.config>: Wi-Fi in the perf kernel (WIFI=1, see README.md).
# Called by ../os/build.sh. Idempotent: the kernel tree persists in the Docker volume.
#  - USB OTG as dual role (dwc2 + usb-role-switch): device (USB console to the Mac) by default,
#    host when the cable carries the RTL8733BU Wi-Fi chip instead (rootfs/usb-role decides).
#  - cfg80211 + Realtek's RTL8733BU vendor driver (vendor/rtl8733bu, GPL-2.0), built in.
#    The firmware is a C array inside the driver: no /lib/firmware files are needed.
set -eu
linux=$1
config=$2
here=$(cd "$(dirname "$0")" && pwd)
drv="$linux/drivers/net/wireless/realtek/rtl8733bu"
rm -rf "$drv"
mkdir -p "$drv"
# The vendor tree without git metadata or host build leftovers.
(cd "$here/vendor/rtl8733bu" && tar cf - --exclude=.git --exclude='*.o' --exclude='*.cmd' .) | (cd "$drv" && tar xf -)
sh "$here/patch-driver.sh" "$drv"
kc="$linux/drivers/net/wireless/realtek/Kconfig"
mk="$linux/drivers/net/wireless/realtek/Makefile"
grep -q rtl8733bu/Kconfig "$kc" || sed -i 's|^source "drivers/net/wireless/realtek/rtl8xxxu/Kconfig"$|&\nsource "drivers/net/wireless/realtek/rtl8733bu/Kconfig"|' "$kc"
grep -q rtl8733bu/Kconfig "$kc"
grep -q 'CONFIG_RTL8733BU' "$mk" || echo 'obj-$(CONFIG_RTL8733BU)	+= rtl8733bu/' >> "$mk"
# The Rockchip USB2 PHY runs its own OTG state machine on the OTG port, with charger
# detection. With the Wi-Fi cable the data lines see a USB device instead of a host; the
# detection then powers the PHY off, its clock stops, and dwc2 cannot enter host mode
# ("dwc2_wait_for_mode: Couldn't set host mode", first Wi-Fi-cable boot, 14 Nov). A port marked
# rockchip,controller-role (perf DTS) skips that state machine: the PHY stays powered as the
# controller set it up, and the role comes only from the dwc2 role switch (rootfs/usb-role).
python3 - "$linux/drivers/phy/rockchip/phy-rockchip-inno-usb2.c" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
if 'rockchip,controller-role' not in s:
    old = """	rport->mode = of_usb_get_dr_mode_by_phy(child_np, -1);
	if (rport->mode == USB_DR_MODE_HOST ||
	    rport->mode == USB_DR_MODE_UNKNOWN) {
		ret = 0;
		goto out;
	}
"""
    assert s.count(old) == 1
    s = s.replace(old, """	rport->mode = of_usb_get_dr_mode_by_phy(child_np, -1);
	/* miracast kernel-wifi.sh: role chosen by the controller's role switch, no PHY OTG/charger state machine */
	if (of_property_read_bool(child_np, "rockchip,controller-role"))
		rport->mode = USB_DR_MODE_UNKNOWN;
	if (rport->mode == USB_DR_MODE_HOST ||
	    rport->mode == USB_DR_MODE_UNKNOWN) {
		ret = 0;
		goto out;
	}
""")
    open(p, 'w').write(s)
PY
grep -q 'rockchip,controller-role' "$linux/drivers/phy/rockchip/phy-rockchip-inno-usb2.c"
# Kernel options. The driver brings its own crypto and regulatory tables; cfg80211 needs no
# regulatory.db (world domain until the driver sets the country).
cat >> "$config" <<'EOF'
CONFIG_USB_DWC2_DUAL_ROLE=y
# CONFIG_USB_DWC2_PERIPHERAL is not set
# CONFIG_USB_DWC2_HOST is not set
CONFIG_USB_ROLE_SWITCH=y
CONFIG_NETDEVICES=y
# CONFIG_ETHERNET is not set
# CONFIG_PHYLIB is not set
# CONFIG_USB_NET_DRIVERS is not set
CONFIG_WIRELESS=y
CONFIG_CFG80211=y
# CONFIG_CFG80211_REQUIRE_SIGNED_REGDB is not set
# CONFIG_CFG80211_DEFAULT_PS is not set
# CONFIG_CFG80211_WEXT is not set
# CONFIG_MAC80211 is not set
# CONFIG_RFKILL is not set
CONFIG_WLAN=y
# CONFIG_WLAN_VENDOR_ADMTEK is not set
# CONFIG_WLAN_VENDOR_ATH is not set
# CONFIG_WLAN_VENDOR_ATMEL is not set
# CONFIG_WLAN_VENDOR_BROADCOM is not set
# CONFIG_WLAN_VENDOR_INTEL is not set
# CONFIG_WLAN_VENDOR_INTERSIL is not set
# CONFIG_WLAN_VENDOR_MARVELL is not set
# CONFIG_WLAN_VENDOR_MEDIATEK is not set
# CONFIG_WLAN_VENDOR_MICROCHIP is not set
# CONFIG_WLAN_VENDOR_PURELIFI is not set
# CONFIG_WLAN_VENDOR_RALINK is not set
# CONFIG_WLAN_VENDOR_RSI is not set
# CONFIG_WLAN_VENDOR_SILABS is not set
# CONFIG_WLAN_VENDOR_ST is not set
# CONFIG_WLAN_VENDOR_TI is not set
# CONFIG_WLAN_VENDOR_ZYDAS is not set
# CONFIG_WLAN_VENDOR_QUANTENNA is not set
CONFIG_WLAN_VENDOR_REALTEK=y
# CONFIG_RTL8XXXU is not set
# CONFIG_RTW88 is not set
# CONFIG_RTW89 is not set
# CONFIG_RTL_CARDS is not set
# CONFIG_RTL8187 is not set
CONFIG_RTL8733BU=y
EOF
