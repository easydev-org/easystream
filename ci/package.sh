#!/bin/sh
# Turn a built easystream into this base's native package(s).
#
#     sh ci/package.sh ubuntu-26.04
#
# WHY A SCRIPT AND NOT THREE STEPS IN THE WORKFLOW
#
# The four Linux jobs have to do the same five things - pick the format that belongs to the
# base, fetch nfpm, set the version, run it, and say what came out - and a workflow that
# repeats them four times is four places to fix the next time a format changes. This is
# that one place.
#
# THE FORMAT IS DECIDED BY THE BASE, NOT BY THE CALLER.
#
#     apt family   -> .deb   (and .apk is meaningless there)
#     fedora       -> .rpm
#     leap         -> .rpm
#     arch/manjaro -> .pkg.tar.zst
#
# Producing every format from every base would be actively wrong: a .deb built on Fedora
# links Fedora's libx264 and would carry a Debian dependency line that describes a library
# it was not linked against.
set -eu

BASE=${1:?usage: package.sh BASE [VERSION]}
VERSION=${2:-${GITHUB_REF_NAME:-0.0.0}}
VERSION=${VERSION#v}

BIN=target/release/easystream
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# nfpm reads `./easystream`; put it where the config expects, keeping the release's own
# name out of the package.
cp "$BIN" ./easystream
chmod +x ./easystream

NFPM_VERSION=2.43.0
if [ ! -x /usr/local/bin/nfpm ]; then
    echo "=== fetching nfpm $NFPM_VERSION ==="
    curl -fsSL "https://github.com/goreleaser/nfpm/releases/download/v${NFPM_VERSION}/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz" \
      | tar -xz -C /usr/local/bin nfpm
    chmod +x /usr/local/bin/nfpm
fi
nfpm --version

case "$BASE" in
    fedora*|leap*) PACKAGER=rpm ;;
    arch*|manjaro*) PACKAGER=archlinux ;;
    *) PACKAGER=deb ;;
esac

echo "=== packaging for $BASE as $PACKAGER ==="
mkdir -p dist
VERSION="$VERSION" nfpm package \
    --config packaging/nfpm.yaml \
    --packager "$PACKAGER" \
    --target "dist/"

# THE FILENAME MUST CARRY THE BASE. Two of these packages are for the same architecture and
# different libraries; without the base in the name a user has no way to pick, and an
# upgrade would silently swap one ABI for another.
for f in dist/*; do
    case "$f" in
        *"$BASE"*) ;;
        *)
            ext=${f##*/}
            base=${ext%.*}
            suffix=${ext#"$base"}
            mv "$f" "dist/${base}-${BASE}${suffix}"
            ;;
    esac
done

echo "=== what was produced ==="
ls -l dist/
echo "=== the metadata a package manager will read ==="
for f in dist/*; do
    case "$f" in
        *.deb) dpkg-deb -I "$f" 2>/dev/null | grep -E 'Package|Version|Depends|Architecture' || true ;;
        *.rpm) rpm -qip "$f" 2>/dev/null | grep -E 'Name|Version|Requires' | head -8 || true ;;
        *.zst) tar -xOf "$f" .PKGINFO 2>/dev/null | grep -E '^(pkgname|pkgver|depend)' || true ;;
    esac
done
