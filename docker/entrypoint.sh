#!/bin/bash
set -e

if [ -d /etc/stroem/startup.d ]; then
  for script in /etc/stroem/startup.d/*.sh; do
    [ -f "$script" ] && source "$script"
  done
fi

exec "$@"
