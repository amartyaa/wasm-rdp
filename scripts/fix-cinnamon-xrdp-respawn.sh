#!/bin/sh
# Keep Cinnamon alive on xrdp when Mesa llvmpipe segfaults.
#
# Why: on a GPU-less xorgxrdp session Cinnamon/Muffin render through Mesa
# llvmpipe. The RandR screen resize that MS-RDPEDISP drives (notably on every
# client reconnect) makes llvmpipe's AVX2 texture gather read freed pages, so
# the llvmpipe-N worker threads take SIGSEGV and `cinnamon` dies. Upstream
# cinnamon-launcher then execs its fallback WM `metacity`, which Ubuntu 26.04
# does not ship -> FileNotFoundError, no window manager, and the session is
# left showing only "You are currently running in fallback mode".
#
# Fix: override cinnamon.desktop (the RequiredComponent cinnamon-session
# launches) so it runs a supervisor that just restarts cinnamon instead.
# Per-user, no root, no package changes.
#
# Usage: ./fix-cinnamon-xrdp-respawn.sh [install|uninstall|test]

set -e

BIN="$HOME/.local/bin/cinnamon-respawn"
DESKTOP="$HOME/.local/share/applications/cinnamon.desktop"
SYS_DESKTOP=/usr/share/applications/cinnamon.desktop

install_it() {
    mkdir -p "${BIN%/*}" "${DESKTOP%/*}"

    cat > "$BIN" <<'SH'
#!/bin/sh
# Restart cinnamon when it dies from a signal; follow it out when it exits cleanly
# (logout, or `cinnamon --replace` from elsewhere) -- same contract as cinnamon-launcher.
n=0
t0=$(date +%s)
while :; do
    cinnamon --replace "$@"
    rc=$?
    [ "$rc" -lt 128 ] && exit "$rc"

    now=$(date +%s)
    [ $((now - t0)) -gt 300 ] && { n=0; t0=$now; }
    n=$((n + 1))
    # ponytail: give up after 5 crashes in 5 min so a hard crash loop doesn't spin forever
    if [ "$n" -gt 5 ]; then
        logger -t cinnamon-respawn "cinnamon crashed $n times in 5 min, giving up (rc=$rc)"
        exit "$rc"
    fi
    logger -t cinnamon-respawn "cinnamon died rc=$rc, restarting"
    sleep 2
done
SH
    chmod +x "$BIN"

    # Copy the distro entry and swap only Exec, so every other key it relies on
    # (X-GNOME-Autostart-Phase, X-GNOME-Provides, ...) stays intact.
    sed "s|^Exec=.*|Exec=$BIN|" "$SYS_DESKTOP" > "$DESKTOP"
    echo "installed $BIN"
    echo "installed $DESKTOP"
    echo "log out of the RDP session and back in to pick it up"
}

uninstall_it() {
    rm -f "$BIN" "$DESKTOP"
    echo "removed override; cinnamon-launcher is back in charge"
}

# Self-check against the live session: kill cinnamon by signal and make sure it
# comes back on its own, with no fallback dialog. SIGKILL rather than SIGSEGV
# because cinnamon catches SIGSEGV and survives a queued one; what the
# supervisor branches on is "died by signal", which SIGKILL reproduces exactly.
test_it() {
    pgrep -f "$BIN" >/dev/null         || { echo "FAIL: supervisor not running -- run '$0 install', then log out and back in"; exit 1; }
    OLD=$(pgrep -x cinnamon | head -1)
    [ -n "$OLD" ] || { echo "FAIL: cinnamon is not running"; exit 1; }

    echo "cinnamon pid $OLD; killing it by signal (stands in for the llvmpipe fault)"
    kill -KILL "$OLD"

    i=0
    while [ $i -lt 40 ]; do
        NEW=$(pgrep -x cinnamon | head -1)
        if [ -n "$NEW" ] && [ "$NEW" != "$OLD" ]; then
            echo "PASS: cinnamon respawned as pid $NEW"
            pgrep -f cinnamon-launcher >/dev/null && echo "WARN: cinnamon-launcher also running"
            exit 0
        fi
        i=$((i + 1))
        sleep 1
    done
    echo "FAIL: cinnamon did not come back"
    exit 1
}

case "${1:-install}" in
    install)   install_it ;;
    uninstall) uninstall_it ;;
    test)      test_it ;;
    *)         echo "usage: $0 [install|uninstall|test]"; exit 1 ;;
esac
