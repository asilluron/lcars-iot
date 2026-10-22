#!/bin/sh
if [ ! -S /run/udev/control ]; then
  /usr/local/bin/bind-udev || echo "lcars: could not mount udev" >&2
fi
exec /usr/local/bin/lcars-app
