#!/usr/bin/env sh
set -eu

# Install the msmtp configuration where msmtp/sendmail look for it.
if [ -n "${MSMTPRC:-}" ] && [ -r "$MSMTPRC" ] && [ "$MSMTPRC" != /etc/msmtprc ]
then
  install -m 400 "$MSMTPRC" /etc/msmtprc
fi

exec /usr/local/bin/luks-ssh-unlock "$@"
