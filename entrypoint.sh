#!/usr/bin/env bash

EXTRA_ARGS=()

if [[ -n "$DEBUG" ]]
then
  EXTRA_ARGS+=(-x)
fi

# Copy the msmtp config file to the default location
if [[ -n "$MSMTPRC" && "$MSMTPRC" != /etc/msmtprc ]]
then
  install -o "$(whoami)" -m 400 "$MSMTPRC" /etc/msmtprc
fi

exec bash "${EXTRA_ARGS[@]}" /luks-ssh-unlock.sh "$@"
