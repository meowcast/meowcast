# Sourced by the host-tool build scripts of this board. Sets:
#   ndk       directory with clang, llvm-objcopy, llvm-objdump, llvm-nm and ld.lld (bare-metal ARM)
#   usb_cflags, usb_libs   libusb-1.0 for the host C++ tools
# MEOW_LLVM (or the older NDK_TOOLS) names the directory; otherwise Homebrew LLVM, then PATH.
# config.env is read here too, so the scripts also work when run directly.
repo=$(pwd); while [ ! -f "$repo/tools/config.sh" ] && [ "$repo" != / ]; do repo=$(dirname "$repo"); done
[ -f "$repo/tools/config.sh" ] && . "$repo/tools/config.sh"
ndk=${MEOW_LLVM:-${NDK_TOOLS:-}}
if [ -z "$ndk" ]; then
    for d in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin; do
        [ -x "$d/clang" ] && [ -x "$d/llvm-objcopy" ] && { ndk=$d; break; }
    done
fi
[ -n "$ndk" ] || ndk=$(dirname "$(command -v llvm-objcopy || echo /nonexistent/x)")
for t in clang llvm-objcopy llvm-objdump llvm-nm; do
    [ -x "$ndk/$t" ] || { echo "toolchain.sh: no $t in '$ndk'. Install LLVM with lld (brew install llvm lld; apt install clang lld llvm) or set MEOW_LLVM in config.env" >&2; exit 2; }
done
if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libusb-1.0; then
    usb_cflags=$(pkg-config --cflags libusb-1.0); usb_libs=$(pkg-config --libs libusb-1.0)
else
    usb_cflags=-I/opt/homebrew/opt/libusb/include/libusb-1.0
    usb_libs='-L/opt/homebrew/opt/libusb/lib -lusb-1.0'
fi
