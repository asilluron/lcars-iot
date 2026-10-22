#!/bin/sh
# Host tuning for the capture thread. Credentials use the same variables as
# the panel; the daemon writes them to tmpfs when they arrive as base64.
for gov in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
  if [ -w "$gov" ]; then
    echo performance > "$gov" || true
  fi
done
for dev in /sys/bus/usb/devices/*; do
  vendor="$dev/idVendor"
  if [ -f "$vendor" ] && grep -q '^2886$' "$vendor"; then
    if [ -w "$dev/power/control" ]; then
      echo on > "$dev/power/control" || true
    fi
    if [ -w "$dev/power/autosuspend" ]; then
      echo -1 > "$dev/power/autosuspend" || true
    fi
  fi
done
mkdir -p /data/wake-log
ulimit -r 50 || true
exec /usr/local/bin/voice-daemon --config /opt/voice/voice.toml
