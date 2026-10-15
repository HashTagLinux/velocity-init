#!/usr/bin/awk -f
# grub-remove.awk — remove a kernel argument from the value of
# GRUB_CMDLINE_LINUX_DEFAULT in /etc/default/grub.
#
# Usage: awk -v tok='init=/sbin/velocity-init' -f grub-remove.awk <grub-file>
#
# Idempotent: removes the token at most once per occurrence and tidies
# any whitespace left behind.
BEGIN {
    if (tok == "") tok = "init=/sbin/velocity-init"
}
{
    if ($0 ~ /^[ \t]*GRUB_CMDLINE_LINUX_DEFAULT[ \t]*=[ \t]*/) {
        if (index($0, tok) == 0) { print; next }
        pre = $0
        sub(/GRUB_CMDLINE_LINUX_DEFAULT[ \t]*=.*/, "GRUB_CMDLINE_LINUX_DEFAULT=", pre)
        val = $0
        sub(/^[ \t]*GRUB_CMDLINE_LINUX_DEFAULT[ \t]*=[ \t]*/, "", val)
        gsub(tok, "", val)
        # collapse runs of whitespace to a single space
        while (index(val, "  ") > 0) gsub(/  /, " ", val)
        # drop whitespace stuck next to a quote / at the edges
        sub(/[ \t]+"/, "\"", val)
        sub(/[ \t]+'/, "'", val)
        sub(/^"[ \t]+/, "\"", val)
        sub(/^'[ \t]+/, "'", val)
        sub(/[ \t]+$/, "", val)
        sub(/^[ \t]+/, "", val)
        print pre val
        next
    }
    print
}