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
# THE FIRST VERSION PRODUCED A 655-BYTE PACKAGE THAT CONTAINED ONLY THE MANIFEST, and this is
# why, because the command looked right:
#
#     pkg create -M "$ROOT/+MANIFEST" -r "$ROOT" -o dist
#
# `-r rootdir` tells pkg-create where the files ARE; it does not tell it WHICH files to
# package. With `-M` and no `-p plist` there is nothing to select from, so it wrote the
# metadata and no payload - and reported success, so the job was green and the artifact was
# empty. The plist is not optional for this mode: it is the file list.
#
# DEPENDENCY NAMES ARE PORTS, NOT LIBRARIES. `pkg install libx264` does not exist; the port
# that installs libx264.so is `x264`. Same for dbus (the port is `dbus`) and libxcb (one port
# installing five shared objects). `pkg create` writes whatever it is told without checking,
# so a wrong name here fails on the user's machine at install time.
set -eu

VERSION=${1:-${GITHUB_REF_NAME:-0.0.0}}
VERSION=${VERSION#v}

BIN=target/release/easystream
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

ROOT=$(pwd)/pkgroot
rm -rf "$ROOT"
mkdir -p "$ROOT/usr/local/bin" "$ROOT/usr/local/share/man/man1" "$ROOT/usr/local/etc"

install -m 0755 "$BIN" "$ROOT/usr/local/bin/easystream"
install -m 0644 packaging/easystream.1 "$ROOT/usr/local/share/man/man1/easystream.1"

cat > "$ROOT/usr/local/etc/easystream-pkg-note" <<'NOTE'
easystream is a per-session daemon. It is not a system service and is not started at boot.

    easystream serve            # est:// on 48079, HTTP on 48078
    easystream serve --help     # options

The daemon needs the session's D-Bus for AT-SPI, and the session's DISPLAY for capture, so it
must run as the logged-in user rather than from /etc/rc.conf.
NOTE

# THE FILE LIST, IN THE LEGACY plist FORMAT.
#
# Paths are ABSOLUTE and relative to the running system, and `-r` is what makes them resolve
# inside the staging tree instead. Directories are listed without a trailing slash; a leading
# @ means a metadata directive (@dir for an empty directory, @mode for permissions).
cat > /tmp/easystream-plist <<'PLIST'
/usr/local/bin/easystream
/usr/local/share/man/man1/easystream.1
/usr/local/etc/easystream-pkg-note
PLIST

# +MANIFEST. `pkg create -M` reads exactly this file.
#
# `arch` is the ABI string from `pkg config abi`, not `amd64`: a package claiming the wrong
# ABI is refused with "wrong architecture", a message that names neither the package's claim
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

mkdir -p dist
# -M manifest, -r staging root, -p file list. All three are required for this mode.
pkg create -M "$ROOT/+MANIFEST" -r "$ROOT" -p /tmp/easystream-plist -o dist --quiet

echo "=== packaged ==="
ls -l dist/

# A PACKAGE THAT CONTAINS ONLY A MANIFEST IS A FAILED BUILD, NOT A SMALL ONE.
#
# The first version of this script produced 655 bytes and every step reported success. A
# size check is crude but it is the difference between noticing in CI and noticing when a
# user runs `pkg install` and gets a daemon that is not there.
PKG=$(ls dist/easystream-*.pkg 2>/dev/null | head -1)
if [ -z "$PKG" ]; then
    echo "=== NO PACKAGE WAS WRITTEN ===" >&2
    exit 1
fi
SIZE=$(stat -f %z "$PKG" 2>/dev/null || stat -c %s "$PKG")
echo "=== $PKG is $SIZE bytes ==="
if [ "$SIZE" -lt 100000 ]; then
    echo "=== THE PACKAGE IS TOO SMALL TO CONTAIN THE BINARY ===" >&2
    pkg info -lF "$PKG" 2>/dev/null | head -10
    exit 1
fi

echo "=== what the package contains ==="
pkg info -lF "$PKG" 2>/dev/null | head -10
echo "=== what it depends on ==="
pkg query -F "$PKG" '%dn-%dv' 2>/dev/null | head -10
