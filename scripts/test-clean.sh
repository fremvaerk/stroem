#!/usr/bin/env bash
set -euo pipefail

MAX_AGE_SECONDS=300
if [ "${1:-}" = "--max-age" ]; then
    case "$2" in
        *h) MAX_AGE_SECONDS=$(( ${2%h} * 3600 )) ;;
        *m) MAX_AGE_SECONDS=$(( ${2%m} * 60 )) ;;
        *s) MAX_AGE_SECONDS=$(( ${2%s} )) ;;
        *) echo "usage: $0 [--max-age <Ns|Nm|Nh>]" >&2; exit 1 ;;
    esac
fi

now=$(date -u +%s)
removed=0
skipped=0

for id in $(docker ps -aq --filter "label=stroem.test=true"); do
    started=$(docker inspect --format '{{.State.StartedAt}}' "$id")
    # Docker's timestamp includes nanoseconds; date(1) wants seconds precision.
    started_epoch=$(date -u -d "${started%.*}Z" +%s 2>/dev/null \
        || date -u -j -f "%Y-%m-%dT%H:%M:%S" "${started%.*}" +%s)
    age=$(( now - started_epoch ))
    name=$(docker inspect --format '{{.Name}}' "$id" | sed 's#^/##')

    if [ "$age" -ge "$MAX_AGE_SECONDS" ]; then
        docker rm -f "$id" >/dev/null
        echo "removed $name (age ${age}s)"
        removed=$((removed + 1))
    else
        echo "skipped $name (age ${age}s, below --max-age ${MAX_AGE_SECONDS}s)"
        skipped=$((skipped + 1))
    fi
done

if [ "$removed" -eq 0 ] && [ "$skipped" -eq 0 ]; then
    echo "nothing to remove — no containers labelled stroem.test=true"
fi
