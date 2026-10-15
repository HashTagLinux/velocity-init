#!/bin/sh
# build-devuan.sh — build the Devuan-flavoured .deb that *replaces* the
# sysvinit PID 1 (as opposed to `build-deb.sh`, which keeps sysvinit and
# boot-switches with a GRUB `init=` argument).
#
# What differs from the generic build:
#   - /sbin/init is a symlink to the velocity binary, so the kernel's
#     no-arg "exec /sbin/init" path already lands on velocity-init.
#   - telinit/reboot/halt/poweroff/shutdown/runlevel symlink to
#     velocityctl (argv0 dispatch), keeping the legacy command set.
#   - control declares Conflicts/Breaks/Replaces on sysvinit-core so a
#     single `dpkg -i` swap uninstalls sysvinit and installs us.
#   - postinst installs /etc/inittab (from the shipped template) when
#     this is the first install.
#
# Usage:  ./pkg/build-devuan.sh [out-dir]
# Env:    VELOCITY_BUILD_ID  version stamp (default <cargo-version>-devuan1)
set -eu

cd "$(dirname "$0")/.."

PKG="velocity-init"
VER="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -n1)"
REV="${VELOCITY_BUILD_ID:-${VER}-devuan1}"
ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"
OUT_DIR="${1:-.}"
REL="target/release"

VELOCITY_BUILD_ID="${REV}" cargo build --release --bins --locked

STAGE="$(mktemp -d /tmp/velocity-devuan.XXXXXX)"
trap 'rm -rf "${STAGE}"' EXIT
R="${STAGE}/pkg"

# ── binaries ────────────────────────────────────────────────────────
install -D -m 0755 "${REL}/velocity-init" "${R}/sbin/velocity-init"
install -D -m 0755 "${REL}/velocityctl"   "${R}/usr/bin/velocityctl"

# ── sysvinit replacement symlinks (argv0 dispatch) ──────────────────
ln -s velocity-init "${R}/sbin/init"
for tool in telinit reboot halt poweroff shutdown runlevel; do
    ln -s velocityctl "${R}/sbin/${tool}"
done
ln -s velocityctl "${R}/usr/bin/runlevel"

# ── config + initramfs shims ────────────────────────────────────────
install -D -m 0644 pkg/inittab.example   "${R}/usr/share/velocity-init/inittab"
install -D -m 0755 pkg/grub-maintain.sh  "${R}/usr/share/velocity-init/grub-maintain.sh"
install -D -m 0755 pkg/grub-append.awk   "${R}/usr/share/velocity-init/grub-append.awk"
install -D -m 0755 pkg/grub-remove.awk   "${R}/usr/share/velocity-init/grub-remove.awk"
install -D -m 0755 debian/velocity-init.hook "${R}/usr/share/initramfs-tools/hooks/velocity-init"
install -D -m 0755 debian/init-bottom "${R}/usr/share/initramfs-tools/scripts/init-bottom/velocity-init"

# ── control / scripts ──────────────────────────────────────────────
mkdir -p "${R}/DEBIAN"
install -m 0755 debian/postinst.devuan "${R}/DEBIAN/postinst"
install -m 0755 debian/prerm           "${R}/DEBIAN/prerm"
cat > "${R}/DEBIAN/control" <<EOF
Package: ${PKG}
Version: ${REV}
Section: admin
Priority: optional
Architecture: ${ARCH}
Maintainer: velocity-init developers <maintainer@example.com>
Installed-Size: $(du -sk "${R}" | cut -f1)
Depends: sysv-rc, initramfs-tools | initramfs-tools-core
Recommends: adduser, procps, kbd
Breaks: sysvinit-core (<< 3.00)
Replaces: sysvinit-core
Conflicts: sysvinit-core, systemd
Description: modern init replacing sysvinit on Devuan
 velocity-init is a Rust PID 1 that boots the classic /etc/inittab
 (rcS / rc N / getty respawns, telinit, runlevels) while supervising
 services with crash restart and cgroup v2 scoping. This Devuan build
 installs /sbin/init -> velocity-init and the telinit/reboot/halt/
 poweroff/shutdown/runlevel argv0 symlinks, and removes sysvinit-core.
EOF

# ── assemble ───────────────────────────────────────────────────────
if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --build --root-owner-group "${R}" "${OUT_DIR}/${PKG}_${REV}_${ARCH}.deb"
else
    CTL="${STAGE}/control.tar.gz"
    DAT="${STAGE}/data.tar.gz"
    tar --sort=name --mtime='1970-01-01 00:00:00Z' --owner=0 --group=0 \
        -czf "${CTL}" -C "${R}/DEBIAN" .
    tar --sort=name --mtime='1970-01-01 00:00:00Z' --owner=0 --group=0 \
        -czf "${DAT}" -C "${R}" --exclude='./DEBIAN' .
    printf '2.0\n' > "${STAGE}/debian-binary"
    ar rcD "${OUT_DIR}/${PKG}_devuan_${REV}_${ARCH}.deb" \
        "${STAGE}/debian-binary" "${CTL}" "${DAT}"
    echo "note: dpkg-deb absent — assembled the .deb manually with ar(1)" >&2
fi

echo "built: ${OUT_DIR}/${PKG}_devuan_${REV}_${ARCH}.deb"