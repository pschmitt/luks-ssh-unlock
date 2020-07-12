#!/usr/bin/env bash

SSH_HOST="${SSH_HOST:-example.com}"
SSH_KEY="${SSH_KEY:-/run/secrets/ssh_key}"
SSH_PORT="${SSH_PORT:-22}"
SSH_USER="${SSH_USER:-root}"

LUKS_PASSWORD="${LUKS_PASSWORD}"
LUKS_PASSWORD_FILE="${LUKS_PASSWORD_FILE=-/run/secrets/luks_password_${SSH_HOST}}"
LUKS_TYPE="direct"

EVENT_FILE="${EVENT_FILE}"
SLEEP_INTERVAL="${SLEEP_INTERVAL:-5}"

HEALTHCHECK_PORT="${HEALTHCHECK_PORT}"

usage() {
  echo "Usage: $(basename "$0") --type LUKS_TYPE --host HOST --port PORT --username USER -- --sleep SLEEP --luks-password PASSWD"
}

log() {
  echo "$(date -Iseconds) $*"
}

if [[ -n "$*" ]]
then
  while true
  do
    case "$1" in
      --help|-h)
        usage
        exit 0
        ;;
      --host|-H)
        SSH_HOST="$2"
        shift 2
        ;;
      --port|-P)
        SSH_PORT="$2"
        shift 2
        ;;
      --username|--user|-u)
        SSH_USER="$2"
        shift 2
        ;;
      --ssh-key|--key|--private-key|--pkey|-k)
        SSH_KEY="$2"
        shift 2
        ;;
      --sleep-interval|--sleep|-s)
        SLEEP_INTERVAL="$2"
        shift 2
        ;;
      --luks-type|--type|-t)
        LUKS_TYPE="$2"
        shift 2
        ;;
      --luks-password|--password|-p)
        LUKS_PASSWORD="$2"
        shift 2
        ;;
      --event-file|--event|-e)
        EVENT_FILE="$2"
        shift 2
        ;;
      *)
        usage >&2
        exit 2
        break
        ;;
    esac
  done
fi

if [[ -n "$DEBUG" ]]
then
  {
    echo "DEBUG: Secrets"
    ls -l /run/secrets
  } >&2
fi

if [[ ! -r "$SSH_KEY" ]]
then
  echo "SSH_KEY at $SSH_KEY is not readable." >&2
  exit 2
fi

# Copy file locally and correct mode
if [[ "$(stat -c "%a" "$SSH_KEY")" != "400" ]]
then
  cp "$SSH_KEY" /tmp
  SSH_KEY="/tmp/$(basename "$SSH_KEY")"
  chmod 400 "$SSH_KEY"
fi

if [[ -n "$LUKS_PASSWORD_FILE" ]]
then
  if [[ ! -r "$LUKS_PASSWORD_FILE" ]]
  then
    echo "$LUKS_PASSWORD_FILE: No such file or directory" >&2
    exit 3
  fi
  LUKS_PASSWORD="$(cat "$LUKS_PASSWORD_FILE")"
fi

if [[ -z "$LUKS_PASSWORD" ]]
then
  echo "LUKS_PASSWORD is not set." >&2
  exit 2
fi

log "LUKS rigmarole started. I'll be trying to unlock ${SSH_HOST}"

while true
do
  # Perform Healthcheck if required
  if [[ -n "$HEALTHCHECK_PORT" ]]
  then
    if nc -z -w 2 "$SSH_HOST" "$HEALTHCHECK_PORT"
    then
      log "Healthcheck result OK"
    fi
    sleep "$SLEEP_INTERVAL"
    continue
  fi

  if nc -z -w 2 "$SSH_HOST" "$SSH_PORT"
  then
    log "Trying to unlock remotely ${SSH_HOST}"

    case "$LUKS_TYPE" in
      direct)
        if timeout 5 sh -c \
          "echo '$LUKS_PASSWORD' | \
           ssh -F /dev/null \
             -o UserKnownHostsFile=/dev/null \
             -o StrictHostKeyChecking=no \
             -t \
             -i '$SSH_KEY' \
             -l '$SSH_USER' \
             '$SSH_HOST'" && [[ -n "$EVENT_FILE" ]]
        then
          mkdir -p "$(dirname "$EVENT_FILE")"
          echo "$(date -Iseconds): LUKS unlocked host at $SSH_HOST" >> "$EVENT_FILE"
        fi
        ;;
      # TODO test
      # https://github.com/gsauthof/dracut-sshd/issues/32
      dracut-systemd|dracut-sshd|dracut|alt)
        timeout 5 sh -c \
          "echo '$LUKS_PASSWORD' | \
           ssh -F /dev/null \
             -o UserKnownHostsFile=/dev/null \
             -o StrictHostKeyChecking=no \
             -t \
             -i '$SSH_KEY' \
             -l '$SSH_USER' \
             '$SSH_HOST' systemd-tty-ask-password-agent"
        ;;
    esac
  else
    log "$SSH_HOST is not reachable on port $SSH_PORT"
  fi

  sleep "$SLEEP_INTERVAL"
done
