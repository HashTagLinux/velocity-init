#!/usr/bin/awk -f
# grub-append.awk — append a kernel argument to the value of
# GRUB_CMDLINE_LINUX_DEFAULT in /etc/default/grub.
#
# Usage: awk -v tok='init=/sbin/velocity-init' -f grub-append.awk <grub-file>
#
# Handles double-quoted, single-quoted and unquoted values. Idempotent:
# if the token is already present anywhere on the line it is left
# untouched.
BEGIN {
    if (tok == "") tok = "init=/sbin/velocity-init"
}
{
    if ($0 ~ /^[ \t]*GRUB_CMDLINE_LINUX_DEFAULT[ \t]*=[ \t]*/) {
        if (index($0, tok) > 0) { print; next }
        pre = $0
        sub(/GRUB_CMDLINE_LINUX_DEFAULT[ \t]*=.*/, "GRUB_CMDLINE_LINUX_DEFAULT=", pre)
        val = $0
        sub(/^[ \t]*GRUB_CMDLINE_LINUX_DEFAULT[ \t]*=[ \t]*/, "", val)
        q = substr(val, 1, 1)
        sep = (val == "" ? "" : " ")
        if (q == "\"" || q == "'") {
            # value is quoted: drop trailing whitespace and the closing
            # quote, append the token, then re-quote
            sub(/[ \t]+$/, "", val)
            val = substr(val, 1, length(val) - 1)
            print pre val sep tok q
        } else {
            # unquoted value
            print pre val sep tok
        }
        next
    }
    print
}
