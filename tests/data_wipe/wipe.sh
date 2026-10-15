#!/bin/sh
### BEGIN INIT INFO
# Provides:          wipe
# Required-Start:
# Required-Stop:
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: deletes init's control socket and journal at boot
### END INIT INFO
case "$1" in
start)
	# Stand-in for mountall-bootclean.sh / tmpfiles --clean: /run is
	# init's directory and the boot scripts are entitled to tidy it.
	# Only the two paths the test handed us are touched, so this is
	# inert on any machine.
	echo "wiping socket ${VELOCITY_TEST_SOCKET}"
	rm -f "${VELOCITY_TEST_SOCKET}"
	echo "wiping journal ${VELOCITY_TEST_JOURNAL}"
	# Segmented layout: wipe both generations (and the base itself,
	# which a real bootclean would also remove had it existed).
	rm -f "${VELOCITY_TEST_JOURNAL}" "${VELOCITY_TEST_JOURNAL}.0" "${VELOCITY_TEST_JOURNAL}.1"
	echo "wipe done"
	;;
stop) ;;
esac
exit 0
