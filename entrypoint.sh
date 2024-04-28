#!/usr/bin/env bash

EXTRA_ARGS=()

if [[ -n "$DEBUG" ]]
then
  EXTRA_ARGS+=(-x)
fi

exec bash "${EXTRA_ARGS[@]}" /luks-ssh-unlock.sh "$@"
