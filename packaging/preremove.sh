#!/bin/sh
# Pre-remove: stop the unit if it is running, so a removal does not leave a captured screen
# behind and a unit that systemd remembers.
#
# THIS IS BEST-EFFORT AND MUST NOT FAIL THE REMOVAL. A user unit belongs to a session that
# the package manager has no handle on; `systemctl --user` from a root context connects to
# root's own user manager, not the logged-in user's. So every command here is guarded and
# the script always exits 0 - a package that refuses to uninstall because a background
# service would not stop is worse than one that leaves a stale unit file.
set -u

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop easystream 2>/dev/null || true
    systemctl --user disable easystream 2>/dev/null || true
fi

exit 0
