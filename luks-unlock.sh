#!/usr/bin/env bash

SSH_HOST="${SSH_HOST:-example.com}"
SSH_KEY="${SSH_KEY:-/run/secrets/ssh_key}"
SSH_PORT="${SSH_PORT:-22}"
SSH_USER="${SSH_USER:-root}"

LUKS_PASSWORD="${LUKS_PASSWORD}"
LUKS_PASSWORD_FILE="${LUKS_PASSWORD_FILE=-/run/secrets/luks_password_${SSH_HOST}}"
LUKS_TYPE="${LUKS_TYPE:-direct}"

EVENTS_FILE="${EVENTS_FILE}"
SLEEP_INTERVAL="${SLEEP_INTERVAL:-5}"

HEALTHCHECK_PORT="${HEALTHCHECK_PORT}"

APPRISE_URL="${APPRISE_URL}"
APPRISE_TAG="${APPRISE_TAG}"
APPRISE_TITLE="${APPRISE_TITLE}"

usage() {
  echo "Usage: $(basename "$0") --type LUKS_TYPE --host HOST --port PORT --username USER -- --sleep SLEEP --luks-password PASSWD"
}

log() {
  echo "$(date -Iseconds) $*"
}

log-notify() {
  local log_event
  log_event="$(log "$@")"

  # Stdout
  echo "$log_event"

  # Events file
  if [[ -n "$EVENTS_FILE" ]]
  then
    mkdir -p "$(dirname "$EVENTS_FILE")"
    echo "$log_event" >> "$EVENTS_FILE"
  fi

  # Apprise
  if [[ -n "$APPRISE_URL" ]]
  then
    local jdata='{"body": "'"${log_event}"'"}'

    if [[ -n "$APPRISE_TAG" ]]
    then
      jdata="$(jq '. + {"tag": "'"${APPRISE_TAG}"'"}' <<< "$jdata")"
    fi

    if [[ -n "$APPRISE_TITLE" ]]
    then
      jdata="$(jq '. + {"title": "'"${APPRISE_TITLE}"'"}' <<< "$jdata")"
    fi

    curl -X POST -H "Content-Type: application/json" \
      -d "$jdata" "$APPRISE_URL"
  fi
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
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
          EVENTS_FILE="$2"
          shift 2
          ;;
        --apprise-url|--apprise|-a)
          APPRISE_URL="$2"
          shift 2
          ;;
        --apprise-tag|--tag)
          APPRISE_TAG="$2"
          shift 2
          ;;
        --apprise-title|--title)
          APPRISE_TITLE="$2"
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

  if [[ -z "$LUKS_PASSWORD" ]] && [[ -n "$LUKS_PASSWORD_FILE" ]]
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
        if [[ -n "$DEBUG" ]]
        then
          log "Healthcheck result OK"
        fi
        sleep "$SLEEP_INTERVAL"
        continue
      fi
    fi

    if nc -z -w 2 "$SSH_HOST" "$SSH_PORT"
    then
      log "Trying to unlock remotely ${SSH_HOST}"

      case "$LUKS_TYPE" in
        direct)
          if echo "$LUKS_PASSWORD" | \
               ssh -F /dev/null \
                 -o ConnectTimeout=5 \
                 -o UserKnownHostsFile=/dev/null \
                 -o StrictHostKeyChecking=no \
                 -t \
                 -i "$SSH_KEY" \
                 -l "$SSH_USER" \
                 "$SSH_HOST"
          then
            log-notify "LUKS unlocked host at $SSH_HOST"
          else
            log-notify "Failed to unlock $SSH_HOST" >&2
          fi
          ;;
        # TODO test
        # https://github.com/gsauthof/dracut-sshd/issues/32
        dracut-systemd|dracut-sshd|dracut|alt)
            if echo "$LUKS_PASSWORD" | \
                 ssh -F /dev/null \
                   -o ConnectTimeout=5 \
                   -o UserKnownHostsFile=/dev/null \
                   -o StrictHostKeyChecking=no \
                   -t \
                   -i "$SSH_KEY" \
                   -l "$SSH_USER" \
                   "$SSH_HOST" systemd-tty-ask-password-agent
          then
            log-notify "LUKS unlocked host at $SSH_HOST"
          else
            log-notify "Failed to unlock $SSH_HOST" >&2
          fi
          ;;
      esac
    else
      log "$SSH_HOST is not reachable on port $SSH_PORT"
    fi

    sleep "$SLEEP_INTERVAL"
  done
fi
