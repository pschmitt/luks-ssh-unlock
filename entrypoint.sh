#!/usr/bin/env bash

if [[ -n "$DEBUG" ]]
then
  set -x
fi

# Copy the msmtp config file to the default location
if [[ -n "$MSMTPRC" && "$MSMTPRC" != /etc/msmtprc ]]
then
  mkdir -p /etc
  install -o "$(whoami)" -m 400 "$MSMTPRC" /etc/msmtprc
fi

exec /usr/local/bin/luks-ssh-unlock.sh "$@"
