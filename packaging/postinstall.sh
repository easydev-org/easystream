#!/bin/sh
# Post-install: say what was installed and what was deliberately NOT started.
#
# A PACKAGE MUST NOT START A SCREEN CAPTURE DAEMON ON ITS OWN.
#
# `systemctl --user enable --now` would be one line here and it would be wrong: easystream
# captures the screen and injects input. Enabling it from a package means a person installs
# something to look at it and finds their desktop being recorded. So this prints the one
# command that starts it and stops there.
set -eu

cat <<'MSG'
easystream is installed but NOT running.

It is a user service, because it needs the session's display and D-Bus:

    systemctl --user enable --now easystream
    xdg-open http://127.0.0.1:48078/

Check what it resolved after starting:

    curl -s http://127.0.0.1:48078/capabilities
MSG

# On Debian and Ubuntu the user manager may not be running for a session that was created
# before systemd's user instance; the unit is still installed and will start at next login.
if command -v systemctl >/dev/null 2>&1; then
    if ! systemctl --user is-system-running >/dev/null 2>&1; then
        echo
        echo "note: no systemd user instance in this context; enable it from a session."
    fi
fi

exit 0
