#!/bin/sh
#
# grub-maintain.sh — shared GRUB_CMDLINE_LINUX_DEFAULT maintenance for
# velocity-init. Sourced by debian/postinst and debian/prerm. Both
# functions are idempotent: no matter how many times they run there is
# exactly one `init=/sbin/velocity-init` kernel argument when enabled,
# and none when disabled.
#
# All of the locations below are overridable so the package scripts can
# be tested without touching a real system.
#
#   VELOCITY_GRUB_CFG      GRUB config file   (default /etc/default/grub)
#   VELOCITY_UPDATE_GRUB   update-grub binary (default update-grub)
#   VELOCITY_GRUB_LIBDIR   awk helper dir     (default /usr/share/velocity-init)

GRUB_CFG="${VELOCITY_GRUB_CFG:-/etc/default/grub}"
UPDATE_GRUB="${VELOCITY_UPDATE_GRUB:-update-grub}"
GRUB_LIBDIR="${VELOCITY_GRUB_LIBDIR:-/usr/share/velocity-init}"
GRUB_INIT_ARG="init=/sbin/velocity-init"

# Add the velocity init= argument. Exit status:
#   0 — nothing changed (already present, or no GRUB config)
#   1 — GRUB config was rewritten and now carries the argument
v_grub_append_init() {
    [ -f "${GRUB_CFG}" ] || return 0
    grep -qF "${GRUB_INIT_ARG}" "${GRUB_CFG}" && return 0
    awk -v tok="${GRUB_INIT_ARG}" \
        -f "${GRUB_LIBDIR}/grub-append.awk" "${GRUB_CFG}" > "${GRUB_CFG}.velocity-tmp" || return 1
    if cmp -s "${GRUB_CFG}" "${GRUB_CFG}.velocity-tmp"; then
        rm -f "${GRUB_CFG}.velocity-tmp"
        return 0
    fi
    chmod 0644 "${GRUB_CFG}.velocity-tmp"
    mv -f "${GRUB_CFG}.velocity-tmp" "${GRUB_CFG}"
    return 1
}

# Remove the velocity init= argument. Exit code:
#   0 = nothing changed (argument absent, or no GRUB config)
#   1 = GRUB config was rewritten (argument removed)
v_grub_remove_init() {
    [ -f "${GRUB_CFG}" ] || return 0
    grep -qF "${GRUB_INIT_ARG}" "${GRUB_CFG}" || return 0
    awk -v tok="${GRUB_INIT_ARG}" \
        -f "${GRUB_LIBDIR}/grub-remove.awk" "${GRUB_CFG}" > "${GRUB_CFG}.velocity-tmp" || return 1
    if cmp -s "${GRUB_CFG}" "${GRUB_CFG}.velocity-tmp"; then
        rm -f "${GRUB_CFG}.velocity-tmp"
        return 0
    fi
    chmod 0644 "${GRUB_CFG}.velocity-tmp"
    mv -f "${GRUB_CFG}.velocity-tmp" "${GRUB_CFG}"
    return 1
}

# Regenerate the bootloader menu only if a GRUB tool is available.
# Never fatal: some systems (LILO, syslinux EFI, …) manage their own
# command line.
v_grub_reapply() {
    # shellcheck disable=SC2086
    if command -v ${UPDATE_GRUB} >/dev/null 2>&1; then
        ${UPDATE_GRUB} >/dev/null 2>&1 || true
    fi
}

# Regenerate the initramfs so the hook ships the (static) binary into
# the early-boot image. Missing update-initramfs is tolerated.
v_initramfs_rebuild() {
    if command -v update-initramfs >/dev/null 2>&1; then
        update-initramfs -u >/dev/null 2>&1 || true
    fi
}