#!/bin/sh
### BEGIN INIT INFO
# Provides:          chatter
# Required-Start:
# Required-Stop:
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: writes more than one log-pipe buffer to stdout
### END INIT INFO
case "$1" in
start)
	# 5000 lines of 64 bytes ≈ 320 KiB: nearly five times the 64 KiB
	# pipe every service gets, so the script is guaranteed to block in
	# write(2) unless the boot path keeps draining it.
	i=0
	while [ "$i" -lt 5000 ]; do
		echo "chatter line $i aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
		i=$((i + 1))
	done
	echo "chatter done"
	;;
stop) ;;
esac
exit 0
