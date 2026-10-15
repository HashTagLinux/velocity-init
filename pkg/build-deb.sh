#!/bin/sh
# build-deb.sh — build the generic velocity-init .deb (non-invasive:
# keeps the existing init and switches at boot with a GRUB `init=`
# argument). For the Devuan build that *replaces* sysvinit with
# /sbin/init fitted directly, use ./pkg/build-devuan.sh instead.
#
# Two assembly strategies:
#   1. `dpkg-deb --build` when the dpkg tools are installed (Debian box).
#   2. a spec-compliant hand-assembly with `ar` + `tar` (works anywhere
#      GNU ar and tar exist — the sandbox case). Both produce a .deb
#      that `dpkg -i` accepts.
#
# The binary is built for the host's glibc, dynamically linked, and
# that is deliberate: an init runs before anything else has started, and
# a dynamically linked one is linked against the exact glibc that will
# be on the machine it boots. Run this on the build box *for the target
# distribution* (a chroot, a container — see docker/Dockerfile.freia);
# building on a newer host and shipping the result there is what makes
# a PID 1 fail with "GLIBC_2.xx not found" at the worst possible
# moment. Only libc.so.6, libgcc_s.so.1 and the loader are needed.
#
# Usage:  ./pkg/build-deb.sh [out-dir]
# Env:    VELOCITY_BUILD_ID  overrides the version stamped into the
#         binary's `--build-id` (default <cargo-version>-1)
set -eu

cd "$(dirname "$0")/.."

PKG="velocity-init"
VER="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -n1)"
REV="${VELOCITY_BUILD_ID:-${VER}-1}"
ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"
OUT_DIR="${1:-.}"
REL="target/release"

VELOCITY_BUILD_ID="${REV}" cargo build --release --bins --locked

STAGE="$(mktemp -d /tmp/velocity-deb.XXXXXX)"
trap 'rm -rf "${STAGE}"' EXIT
DEB_ROOT="${STAGE}/pkg"

# ── payload ────────────────────────────────────────────────────────
install -D -m 0755 "${REL}/velocity-init" "${DEB_ROOT}/sbin/velocity-init"
install -D -m 0755 "${REL}/velocityctl"   "${DEB_ROOT}/usr/bin/velocityctl"
install -D -m 0755 pkg/grub-maintain.sh   "${DEB_ROOT}/usr/share/velocity-init/grub-maintain.sh"
install -D -m 0755 pkg/grub-append.awk    "${DEB_ROOT}/usr/share/velocity-init/grub-append.awk"
install -D -m 0755 pkg/grub-remove.awk    "${DEB_ROOT}/usr/share/velocity-init/grub-remove.awk"
install -D -m 0755 debian/velocity-init.hook "${DEB_ROOT}/usr/share/initramfs-tools/hooks/velocity-init"
install -D -m 0755 debian/init-bottom     "${DEB_ROOT}/usr/share/initramfs-tools/scripts/init-bottom/velocity-init"

# ── control ────────────────────────────────────────────────────────
mkdir -p "${DEB_ROOT}/DEBIAN"
install -m 0755 debian/postinst "${DEB_ROOT}/DEBIAN/postinst"
install -m 0755 debian/prerm    "${DEB_ROOT}/DEBIAN/prerm"
cat > "${DEB_ROOT}/DEBIAN/control" <<EOF
Package: ${PKG}
Version: ${REV}
Section: admin
Priority: optional
Architecture: ${ARCH}
Maintainer: velocity-init developers <maintainer@example.com>
Installed-Size: $(du -sk "${DEB_ROOT}" | cut -f1)
Depends: initramfs-tools | initramfs-tools-core, adduser
Recommends: procps, kbd
Description: modern dependency-based init system and service manager
 velocity-init is a Rust PID 1 that boots LSB-header init scripts in
 parallel from a dependency graph and supervises services with crash
 restart, backoff, cgroup v2 scoping, socket activation and an operator
 control socket. It is also a full /etc/inittab drop-in for Devuan
 (telinit, runlevel, who -r, ctrl-alt-del and power signals).
 .
 Select it at boot with init=/sbin/velocity-init (added to
 GRUB_CMDLINE_LINUX_DEFAULT by postinst).
EOF

# ── assemble ───────────────────────────────────────────────────────
if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --build --root-owner-group "${DEB_ROOT}" "${OUT_DIR}/${PKG}_${REV}_${ARCH}.deb"
else
    # hand-rolled .deb: three ar members, in the expected order.
    CTL="${STAGE}/control.tar.gz"
    DAT="${STAGE}/data.tar.gz"
    tar --sort=name --mtime='1970-01-01 00:00:00Z' --owner=0 --group=0 \
        -czf "${CTL}" -C "${DEB_ROOT}/DEBIAN" .
    tar --sort=name --mtime='1970-01-01 00:00:00Z' --owner=0 --group=0 \
        -czf "${DAT}" -C "${DEB_ROOT}" --exclude='./DEBIAN' .
    printf '2.0\n' > "${STAGE}/debian-binary"
    ar rcD "${OUT_DIR}/${PKG}_${REV}_${ARCH}.deb" \
        "${STAGE}/debian-binary" "${CTL}" "${DAT}"
    echo "note: dpkg-deb absent — assembled the .deb manually with ar(1)" >&2
fi

echo "built: ${OUT_DIR}/${PKG}_${REV}_${ARCH}.deb"