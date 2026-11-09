#!/bin/sh
# Stand-in for rabbitmqctl (-Drabbitmqctl.bin): roll has no rabbitmqctl/node control.
# Tests that need node control are listed in excluded.txt. The only commands accepted are
# the teardown helpers that restore defaults (unblock / clear alarms), which are no-ops on
# a broker that was never blocked. Everything else fails loudly.
[ "${1:-}" = "-n" ] && shift 2
args="$*"
case "$args" in
  "set_vm_memory_high_watermark 0.4" | "eval rabbit_alarm:clear_alarm("*) exit 0 ;;
esac
echo "rabbitmqctl is not available against roll: $args" >&2
exit 1
