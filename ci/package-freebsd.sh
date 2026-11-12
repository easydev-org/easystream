#!/bin/sh
# Turn the FreeBSD build into a native .pkg, using FreeBSD's own pkg-create.
#
#     sh ci/package-freebsd.sh 15.1
#
# WHY NOT nfpm LIKE THE LINUX JOBS
#
# nfpm writes deb, rpm, apk and archlinux packages. It has no FreeBSD backend, so the Linux
# script cannot be reused: a .pkg is a tar with a +MANIFEST and a +COMPACT_MANIFEST, and
# `pkg create` is the tool the system already has.
#
# THE DEPENDENCY NAMES ARE THE PORTS, NOT THE PACKAGE NAMES ON DISK. `pkg install libx264`
# does not exist; the port that provides libx264.so is `x264`. Same for the X libraries:
# libxcb is one port that installs all five shared objects this binary links. Getting these
# wrong does not fail the build - `pkg create` writes whatever it is told - it fails on the
# user's machine at install time, which is far worse.
set -eu

VERSION=${1:-${GITHUB_REF_NAME:-0.0.0}}
VERSION=${VERSION#v}

BIN=target/release/easystream
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

ROOT=$(pwd)/pkgroot
rm -rf "$ROOT"
mkdir -p "$ROOT/usr/local/bin" "$ROOT/usr/local/share/man/man1" \
         "$ROOT/usr/local/lib/systemd/user" "$ROOT/usr/local/etc"

install -m 0755 "$BIN" "$ROOT/usr/local/bin/easystream"
install -m 0644 packaging/easystream.1 "$ROOT/usr/local/share/man/man1/easystream.1"
install -m 0644 packaging/easystream.service \
    "$ROOT/usr/local/lib/systemd/user/easystream.service"

# THE POST-INSTALL SCRIPT, RUN FROM THE PACKAGE'S OWN PREFIX.
#
# The Linux script only calls `ldconfig`-equivalents that do not exist here, and the service
# is a user unit in a path FreeBSD does not have. These two lines are the whole of it: pkg
# needs nothing else, and a FreeBSD user enables the daemon the same way a Linux user does.
cat > "$ROOT/usr/local/etc/easystream-pkg-note" <<'NOTE'
easystream is a per-session daemon. It is not a system service and is not started at boot.

    easystream serve            # est:// on 48079, HTTP on 48078
    easystream serve --help     # options

The daemon needs the session's D-Bus for AT-SPI, and the session's DISPLAY for capture, so it
must run as the logged-in user rather than from /etc/rc.conf.
NOTE

# +MANIFEST. `pkg create -M` reads exactly this file, and every key below is required except
# the ones FreeBSD's own portsleave out deliberately.
#
# `arch` is the ABI string from `pkg config abi`, not `amd64`: a package claiming the wrong
# ABI is refused with "wrong architecture" and the message names neither the package's claim
# nor the system's.
ABI=$(pkg config abi)
cat > "$ROOT/+MANIFEST" <<MANIFEST
name: easystream
version: "${VERSION}"
origin: local/easystream
comment: "Screen capture, input injection and accessibility over est://"
www: "https://github.com/easydev-org/easystream"
maintainer: "dev@easydev.org"
prefix: /usr/local
abi: ${ABI}
desc: <<EOD
The easystream daemon: screen capture, input injection, an accessibility tree and a web
UI, over one process. A viewer or the easyworker connects to it; this package installs the
daemon. It is the FreeBSD member of a family that implements the same est:// protocol
natively on Windows, macOS and Linux.
EOD
categories: [sysutils]
licenselogic: single
licenses: [MIT]

# THE PORTS THESE SHARED OBJECTS COME FROM, WHICH IS NOT ALWAYS THE LIBRARY'S NAME.
#
#   x264      installs libx264.so.NN      - the port is `x264`
#   libyuv    installs libyuv.so          - port name is the same
#   opus      installs libopus.so.NN      - the port is `opus`
#   libxcb    installs libxcb.so plus xcb-xtest/-shm/-xfixes/-render
#   libX11    installs libX11.so
#   dbus      installs libdbus-1.so       - the port is `dbus`, not `dbus-libs`
#   at-spi2-core  provides the D-Bus accessibility bus, not a link dependency
deps: {
    x264: {origin: "multimedia/libx264", version: "0"},
    libyuv: {origin: "graphics/libyuv", version: "0"},
    opus: {origin: "audio/opus", version: "0"},
    libxcb: {origin: "x11/libxcb", version: "0"},
    libX11: {origin: "x11/libX11", version: "0"},
    dbus: {origin: "devel/dbus", version: "0"},
    at-spi2-core: {origin: "accessibility/at-spi2-core", version: "0"}
}
MANIFEST

# +COMPACT_MANIFEST is the small one `pkg` reads first; `pkg create` derives it from
# +MANIFEST when -M is used, so there is nothing to write here.
mkdir -p dist
# -o is the output DIRECTORY, and the filename comes from `name` and `version` above.
pkg create -M "$ROOT/+MANIFEST" -r "$ROOT" -o dist --quiet

echo "=== packaged ==="
ls -l dist/
# THE MANIFEST AS pkg SEES IT, which is the only way to prove the dependencies landed.
echo "=== what the package declares ==="
for f in dist/easystream-*.pkg; do
    [ -e "$f" ] || continue
    pkg info -lF "$f" 2>/dev/null | head -8
    echo "  --- deps ---"
    pkg query -F "$f" '%dn-%dv' 2>/dev/null | head -10
done
