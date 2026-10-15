#!/bin/sh
# test-maintainers.sh — verify the package maintainer scripts and the
# GRUB/initramfs integration WITHOUT touching a real system.
#
#   - fake /etc/default/grub (VELOCITY_GRUB_CFG)
#   - fake `update-grub` that just counts invocations
#   - shim dir override for the awk helpers (VELOCITY_GRUB_LIBDIR)
#   - fake initramfs staging root ($DESTDIR) for the hook
#
# Run:  ./pkg/test-maintainers.sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/velocity-maint.XXXXXX)"
trap 'rm -rf "${T}"' EXIT

pass=0; fail=0
ok()  { printf 'ok   %s\n' "$1"; pass=$((pass+1)); }
bad() { printf 'FAIL %s\n' "$1"; fail=$((fail+1)); }

# ── environment under test ──────────────────────────────────────────
GRUB="$T/grub"
COUNT="$T/update-grub.count"
export VELOCITY_GRUB_CFG="$GRUB"
export VELOCITY_UPDATE_GRUB="$T/fake-update-grub"
export VELOCITY_GRUB_LIBDIR="$ROOT/pkg"
export DESTDIR="$T/initramfs"

cat > "$T/fake-update-grub" <<EOF
#!/bin/sh
n=0
[ -f "$COUNT" ] && n=\$(cat "$COUNT")
echo \$((n+1)) > "$COUNT"
EOF
chmod +x "$T/fake-update-grub"

cat > "$GRUB" <<'EOF'
# GRUB boot loader configuration
GRUB_DEFAULT=0
GRUB_TIMEOUT=5
GRUB_CMDLINE_LINUX_DEFAULT="quiet splash"
GRUB_CMDLINE_LINUX=""
EOF

grub_count() { cat "$COUNT" 2>/dev/null || echo 0; }

# ── 1: postinst configure (fresh install) ───────────────────────────
"$ROOT/debian/postinst" configure >/dev/null 2>&1
grep -qF 'init=/sbin/velocity-init' "$GRUB" \
    && ok "postinst added init= to GRUB" || bad "postinst did not add init="
[ "$(grub_count)" -eq 1 ] && ok "update-grub ran once" || bad "update-grub ran $(grub_count)x (want 1)"

# ── 2: idempotency — reinstall ──────────────────────────────────────
"$ROOT/debian/postinst" configure >/dev/null 2>&1
[ "$(grep -cF 'init=/sbin/velocity-init' "$GRUB")" -eq 1 ] \
    && ok "postinst idempotent (exactly one arg)" || bad "duplicate init= args after reinstall"
[ "$(grub_count)" -eq 1 ] && ok "no update-grub churn on reinstall" || bad "update-grub re-ran on reinstall"

# ── 3: prerm remove (uninstall) ─────────────────────────────────────
"$ROOT/debian/prerm" remove >/dev/null 2>&1
grep -qF 'init=/sbin/velocity-init' "$GRUB" \
    && bad "init= left after prerm" || ok "prerm removed init="
[ "$(grub_count)" -eq 2 ] && ok "update-grub ran once on prerm" || bad "update-grub count $(grub_count) after prerm"

# ── 4: prerm idempotency — second run must not churn ────────────────
"$ROOT/debian/prerm" remove >/dev/null 2>&1
[ "$(grub_count)" -eq 2 ] && ok "prerm idempotent (no churn)" || bad "prerm re-ran update-grub"

# ── 5: postinst on a GRUB that has never seen velocity-init ─────────
printf 'GRUB_DEFAULT=0\nGRUB_TIMEOUT=5\nGRUB_CMDLINE_LINUX_DEFAULT="quiet"\n' > "$T/grub-bare"
VELOCITY_GRUB_CFG="$T/grub-bare" "$ROOT/debian/postinst" configure >/dev/null 2>&1
grep -qF 'init=/sbin/velocity-init' "$T/grub-bare" \
    && ok "postinst works on bare grub" || bad "bare grub untouched"

# ── 6: initramfs hook (fallback branch, no hook-functions on host) ──
mkdir -p "$T/srcbin"
cp "$ROOT/target/release/velocity-init" "$T/srcbin/velocity-init"
cp "$ROOT/target/release/velocityctl"    "$T/srcbin/velocityctl"
VELOCITY_INIT_BIN="$T/srcbin/velocity-init" VELOCITYCTL_BIN="$T/srcbin/velocityctl" \
    sh "$ROOT/debian/velocity-init.hook" run
[ -x "$DESTDIR/sbin/velocity-init" ] \
    && ok "hook shipped init binary (exec)" || bad "hook missed sbin/velocity-init"
[ -x "$DESTDIR/usr/bin/velocityctl" ] \
    && ok "hook shipped velocityctl (exec)" || bad "hook missed velocityctl"

# ── 7: init-bottom — accepts our init=, ignores foreign inits ───────
shipped_bin="$DESTDIR/sbin/velocity-init"
VELOCITY_BIN_CHECK="$shipped_bin" VELOCITY_CMDLINE="root=/dev/sda1 init=/sbin/velocity-init" \
    sh "$ROOT/debian/init-bottom" run >/dev/null 2>&1 \
    && ok "init-bottom accepts init=/sbin/velocity-init" || bad "init-bottom rejected our init="
VELOCITY_BIN_CHECK="$shipped_bin" VELOCITY_CMDLINE="root=/dev/sda1 init=/sbin/systemd" \
    sh "$ROOT/debian/init-bottom" run >/dev/null 2>&1 \
    && ok "init-bottom ignores foreign init=" || bad "init-bottom reacted to systemd"

echo
echo "summary: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ]