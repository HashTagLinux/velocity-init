#!/bin/sh
### BEGIN INIT INFO
# Provides:          after
# Required-Start:    chatter
# Required-Stop:
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: runs after the chatty service
### END INIT INFO
case "$1" in
start) echo "after started" ;;
stop) ;;
esac
exit 0
