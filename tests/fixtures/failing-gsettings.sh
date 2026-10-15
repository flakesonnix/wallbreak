#!/bin/sh
set -eu
"$WALLBREAK_TEST_REAL_GSETTINGS" "$@"
if [ "$1" = set ] && [ "$3" = picture-options ] && [ ! -e "$WALLBREAK_TEST_FAILURE_MARKER" ]; then
    touch "$WALLBREAK_TEST_FAILURE_MARKER"
    printf '%s\n' 'synthetic post-write failure' >&2
    exit 1
fi
