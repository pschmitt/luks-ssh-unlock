#!/usr/bin/env bash

SSH_HOSTNAME="${SSH_HOSTNAME:-example.com}"
SSH_KEY="${SSH_KEY:-/run/secrets/ssh_key}"
SSH_PORT="${SSH_PORT:-22}"
SSH_USERNAME="${SSH_USERNAME:-root}"
SSH_CONNECTION_TIMEOUT="${SSH_CONNECTION_TIMEOUT:-5}"

FORCE_IPV4="${FORCE_IPV4:-}"
FORCE_IPV6="${FORCE_IPV6:-}"

SSH_JUMPHOST="${SSH_JUMPHOST:-}"
SSH_JUMPHOST_USERNAME="${SSH_JUMPHOST_USERNAME:-root}"
SSH_JUMPHOST_PORT="${SSH_JUMPHOST_PORT:-${SSH_PORT}}"
SSH_JUMPHOST_KEY="${SSH_JUMPHOST_KEY:-${SSH_KEY}}"

LUKS_PASSWORD="${LUKS_PASSWORD:-}"
LUKS_PASSWORD_FILE="${LUKS_PASSWORD_FILE=-/run/secrets/luks_password_${SSH_HOSTNAME}}"
LUKS_TYPE="${LUKS_TYPE:-direct}"

DEBUG="${DEBUG:-}"
EVENTS_FILE="${EVENTS_FILE:-}"
SKIP_SSH_PORT_CHECK="${SKIP_SSH_PORT_CHECK:-}"
SLEEP_INTERVAL="${SLEEP_INTERVAL:-10}"

HEALTHCHECK_PORT="${HEALTHCHECK_PORT:-}"
HEALTHCHECK_REMOTE_CMD="${HEALTHCHECK_REMOTE_CMD:-}"
HEALTHCHECK_REMOTE_HOSTNAME="${HEALTHCHECK_REMOTE_HOSTNAME:-}"
HEALTHCHECK_REMOTE_USERNAME="${HEALTHCHECK_REMOTE_USERNAME:-}"

APPRISE_TAG="${APPRISE_TAG:-}"
APPRISE_TITLE="${APPRISE_TITLE:-}"
APPRISE_URL="${APPRISE_URL:-}"

EMAIL_FROM="${EMAIL_FROM:-}"
EMAIL_RECIPIENT="${EMAIL_RECIPIENT:-}"
EMAIL_SUBJECT="${EMAIL_SUBJECT:-}"

usage() {
  echo "Usage: $(basename "$0") OPTIONS"
  echo
  echo "Options:"
  echo
  echo "  --help, -h     Display this help message"
  echo "  --debug, -D    Enable debug mode"
  echo "                 Env var: DEBUG"
  echo

  echo "  --host, --ssh-host, -H HOSTNAME"
  echo "                 SSH hostname to connect to"
  echo "                 Env var: SSH_HOSTNAME"
  echo "  --port, --ssh-port PORT"
  echo "                 SSH port to connect to"
  echo "                 Env var: SSH_PORT"
  echo "  --username, --user, -u USERNAME"
  echo "                 SSH username to connect with"
  echo "                 Env var: SSH_USERNAME"
  echo "  --ssh-key, --key, --private-key, --pkey KEY"
  echo "                 SSH private key to use"
  echo "                 Env var: SSH_KEY"
  echo "  --force-ipv4, --ipv4, -4"
  echo "                 Force IPv4"
  echo "                 Env var: FORCE_IPV4"
  echo "  --force-ipv6, --ipv6, -6"
  echo "                 Force IPv6"
  echo "                 Env var: FORCE_IPV6"
  echo

  echo "  --ssh-jumphost, --jumphost, -J JUMPHOST"
  echo "                 SSH jumphost to connect through"
  echo "                 Env var: SSH_JUMPHOST"
  echo "  --ssh-jumphost-username, --jusername, --ju, -U USERNAME"
  echo "                 SSH jumphost username to connect with"
  echo "                 Env var: SSH_JUMPHOST_USERNAME"
  echo "  --ssh-jumphost-port, --jport, --jp PORT"
  echo "                 SSH jumphost port to connect to"
  echo "                 Env var: SSH_JUMPHOST_PORT"
  echo "  --ssh-jumphost-key, --jkey, --jk, -K KEY"
  echo "                 SSH jumphost private key to use"
  echo "                 Env var: SSH_JUMPHOST_KEY"
  echo

  echo "  --event-file, --event, -e FILE"
  echo "                 File to write events to"
  echo "                 Env var: EVENTS_FILE"
  echo "  --sleep-interval, --sleep, --interval, -i SECONDS"
  echo "                 Sleep interval between attempts"
  echo "                 Env var: SLEEP_INTERVAL"
  echo "  --skip-ssh-port-check, --skip-port-check"
  echo "                 Skip SSH port check"
  echo "                 Env var: SKIP_SSH_PORT_CHECK"
  echo

  echo "  --luks-type, --type, -t TYPE"
  echo "                 LUKS type to use (direct, systemd-tool, arch, dracut-systemd, dracut-sshd, dracut, alt)"
  echo "                 Env var: LUKS_TYPE"
  echo "  --luks-password, --password, -p PASSWORD"
  echo "                 LUKS password to use"
  echo "                 Env var: LUKS_PASSWORD"
  echo

  echo "  --remote-check, --healthcheck-remote-cmd, --remote-command, --remote-cmd, --rcmd CMD"
  echo "                 Remote command to check if the host is reachable"
  echo "                 Env var: HEALTHCHECK_REMOTE_CMD"
  echo "  --healthcheck-host, --hc-host HOSTNAME"
  echo "                 Remote hostname to check if the host is reachable"
  echo "                 Env var: HEALTHCHECK_REMOTE_HOSTNAME"
  echo "  --healthcheck-user, --hc-user USERNAME"
  echo "                 Remote username to check if the host is reachable"
  echo "                 Env var: HEALTHCHECK_REMOTE_USERNAME"
  echo

  echo "  --apprise-url, --apprise, -a URL"
  echo "                 Apprise URL to send notifications to"
  echo "                 Env var: APPRISE_URL"
  echo "  --apprise-tag, --tag TAG"
  echo "                 Apprise tag to use"
  echo "                 Env var: APPRISE_TAG"
  echo "  --apprise-title, --title TITLE"
  echo "                 Apprise title to use"
  echo "                 Env var: APPRISE_TITLE"
  echo "  --email-recipient, --email-to EMAIL"
  echo "                 Email address to send notifications to"
  echo "                 Env var: EMAIL_RECIPIENT"
  echo "  --email-from, --email-sender EMAIL"
  echo "                 Email sender address (FROM)"
  echo "                 Env var: EMAIL_FROM"
  echo "  --email-subject SUBJECT"
  echo "                 Email subject to use"
  echo "                 Env var: EMAIL_SUBJECT"
}

log() {
  echo "$(date -Iseconds) $*"
}

template-msg() {
  local msg="$*"

  msg=${msg//#self/$(basename "$0")}
  msg=${msg//#event_type/${event_type}}
  msg=${msg//#event/${event}}
  msg=${msg//#sleep_interval/${SLEEP_INTERVAL}}

  msg=${msg//#hostname/${SSH_HOSTNAME}}
  msg=${msg//#username/${SSH_USERNAME}}
  msg=${msg//#port/${SSH_PORT}}

  msg=${msg//#jumphost/${SSH_JUMPHOST}}
  msg=${msg//#jusername/${SSH_JUMPHOST_USERNAME}}
  msg=${msg//#jport/${SSH_JUMPHOST_PORT}}

  msg=${msg//#luks_type/${LUKS_TYPE}}
  if [[ -n "$DEBUG" ]]
  then
    msg=${msg//#luks_password/${LUKS_PASSWORD}}
  fi

  msg=${msg//#remote_cmd/${HEALTHCHECK_REMOTE_CMD}}
  msg=${msg//#remote_hostname/${HEALTHCHECK_REMOTE_HOSTNAME}}
  msg=${msg//#remote_username/${HEALTHCHECK_REMOTE_USERNAME}}

  echo -n "$msg"
}

log-notify() {
  local event_type

  case "$1" in
    -i|--info)
      event_type=info
      shift
      ;;
    -s|--success)
      event_type=success
      shift
      ;;
    -w|--warning)
      event_type=warning
      shift
      ;;
    -f|--failure|-e|--error)
      event_type=failure
      shift
      ;;
    *)
      event_type=info
      ;;
  esac

  local event="$*"

  # Stdout
  log "$event"

  # Events file
  if [[ -n "$EVENTS_FILE" ]]
  then
    mkdir -p "$(dirname "$EVENTS_FILE")"
    echo "$event" >> "$EVENTS_FILE"
  fi

  # Apprise
  if [[ -n "$APPRISE_URL" ]]
  then
    local jdata
    jdata=$(jq -Mcn \
      --arg event "$event" \
      --arg event_type "$event_type" \
      --arg tag "$APPRISE_TAG" \
      --arg title "$(template-msg "$APPRISE_TITLE")" '
        {
          body: $event,
          type: $event_type
        }
        | if $tag != "" then .tag = $tag else . end
        | if $title != "" then .title = $title else . end
      ')

    curl -fsSL -X POST \
      -H "Content-Type: application/json" \
      -d "$jdata" "$APPRISE_URL"
    return "$?"
  fi

  # email
  if [[ -n "$EMAIL_RECIPIENT" ]]
  then
    if ! command -v sendmail &>/dev/null
    then
      echo "sendmail is not available" >&2
      return 1
    fi

    {
      if [[ -n "$EMAIL_FROM" ]]
      then
        echo "From: $EMAIL_FROM"
      fi

      echo "To: $EMAIL_RECIPIENT"

      if [[ -n "$EMAIL_SUBJECT" ]]
      then
        echo "Subject: $(template-msg "$EMAIL_SUBJECT")"
      fi

      # body
      echo "Event type: ${event_type^^}"  # uppercase
      echo
      echo "$event"

      if [[ -n "$DEBUG" ]]
      then
        echo
        echo "Script: $0"
        echo "Env:"
        printenv
      fi

    } | sendmail "$EMAIL_RECIPIENT"
  fi
}

_ssh() {
  local extra_args=()
  local ssh_opts=(
    -o UserKnownHostsFile=/dev/null
    -o StrictHostKeyChecking=no
    -o ControlMaster=no
  )

  if [[ -n "$FORCE_IPV4" ]]
  then
    ssh_opts+=(-4)
  elif [[ -n "$FORCE_IPV6" ]]
  then
    ssh_opts+=(-6)
  fi

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    # We can't use JumpHost here since it does not inherit the
    # StrictHostKeyChecking settings etc
    extra_args=(-o "ProxyCommand=ssh ${ssh_opts[*]} -W %h:%p -p '${SSH_JUMPHOST_PORT}' -i '${SSH_JUMPHOST_KEY}' '${SSH_JUMPHOST_USERNAME}@${SSH_JUMPHOST}'")
  fi

  ssh -F /dev/null \
    -o ConnectTimeout="$SSH_CONNECTION_TIMEOUT" \
    "${ssh_opts[@]}" \
    -i "$SSH_KEY" \
    -l "$SSH_USERNAME" \
    "${extra_args[@]}" \
    "$SSH_HOSTNAME" \
    "$@"
}

_ssh_jumphost() {
  local ssh_opts=(
    -o UserKnownHostsFile=/dev/null
    -o StrictHostKeyChecking=no
    -o ControlMaster=no
  )

  ssh -F /dev/null \
    -o ConnectTimeout="$SSH_CONNECTION_TIMEOUT" \
    "${ssh_opts[@]}" \
    -i "$SSH_JUMPHOST_KEY" \
    -l "$SSH_JUMPHOST_USERNAME" \
    "$SSH_JUMPHOST" \
    "$@"
}

is-an-ip-address() {
  local n="$1"

  if [[ -z "$n" && ! -t 0 ]]
  then
    n="$(cat)"
  fi

  # ipv4
  if [[ "$n" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]
  then
    return 0
  fi

  # ipv6
  if [[ "$n" =~ ^[0-9a-fA-F:]+$ ]]
  then
    return 0
  fi

  return 1
}

resolve-hostname() {
  if is-an-ip-address "$SSH_HOSTNAME"
  then
    echo "$SSH_HOSTNAME"
    return 0
  fi

  local cmd=(dig +short)

  if [[ -n "$FORCE_IPV4" ]]
  then
    cmd+=(A)
  elif [[ -n "$FORCE_IPV6" ]]
  then
    cmd+=(AAAA)
  fi

  cmd+=("$SSH_HOSTNAME")

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    cmd=(_ssh_jumphost "${cmd[@]}")
  fi

  "${cmd[@]}" | head -1
}

check_ssh_port() {
  # NOTE We resolve the hostname of the target ssh host to work around the fact
  # that some implementations of nc do not support the -4 and -6 flags.
  local resolved_hostname
  resolved_hostname=$(resolve-hostname)

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    echo | _ssh_jumphost nc "$resolved_hostname" "$SSH_PORT" 2>&1 | \
      grep -iE "^SSH-"
    return "$?"
  fi

  nc -z -w 2 "$resolved_hostname" "$SSH_PORT"
}

luks_unlock() {
  case "$LUKS_TYPE" in
    direct)
      _ssh <<< "$LUKS_PASSWORD"
      ;;
    systemd-tool|arch)
      local disk
      local mapper

      mapper=$(_ssh \
        systemctl --no-pager list-unit-files | \
        sed -nr 's/^systemd-cryptsetup@(.+).service.*/\1/p')

      if [[ -z "$mapper" ]]
      then
        echo "Failed to determine root mapper name" >&2
        return 1
      fi

      disk=$(_ssh \
        systemctl cat --no-pager "systemd-cryptsetup@${mapper}" | \
        sed -nr "s;ExecStart=(.+)(/dev/disk/by-uuid/[^ '\"]+).*;\2;p")

      if [[ -z "$disk" ]]
      then
        echo "Failed to determine root disk path" >&2
        return 1
      fi

      if ! _ssh cryptsetup luksOpen "$disk" "$mapper" - <<< "$LUKS_PASSWORD"
      then
        echo "Failed to unlock disk $disk" >&2
        return 1
      fi

      _ssh systemctl restart "systemd-cryptsetup@${mapper}"
      ;;

    # https://github.com/gsauthof/dracut-sshd/issues/32
    dracut-systemd|dracut-sshd|dracut|alt)
      _ssh -tt systemd-tty-ask-password-agent <<< "$LUKS_PASSWORD"
      ;;

    # https://github.com/pschmitt/luks-mount.sh
    luks-mount)
      _ssh luks-mount <<< "$LUKS_PASSWORD"
      ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  while [[ -n "$*" ]]
  do
    case "$1" in
      --help|-h)
        usage
        exit 0
        ;;
      --debug|-D)
        DEBUG=1
        shift
        ;;
      --host|-H|--ssh-host*)
        SSH_HOSTNAME="$2"
        shift 2
        ;;
      --port|-P|--ssh-port)
        SSH_PORT="$2"
        shift 2
        ;;
      --username|--user|-u|--ssh-user*)
        SSH_USERNAME="$2"
        shift 2
        ;;
      --ssh-key|--key|--private-key|--pkey|-k)
        SSH_KEY="$2"
        shift 2
        ;;
      --force-ipv4|--ipv4|-4)
        FORCE_IPV4=1
        shift
        ;;
      --force-ipv6|--ipv6|-6)
        FORCE_IPV6=1
        shift
        ;;
      --ssh-jumphost|--jumphost|-J)
        SSH_JUMPHOST="$2"
        shift 2
        ;;
      --ssh-jumphost-username|--jusername|--ju|-U)
        SSH_JUMPHOST_USERNAME="$2"
        shift 2
        ;;
      --ssh-jumphost-port|--jport|--jp)
        SSH_JUMPHOST_PORT="$2"
        shift 2
        ;;
      --ssh-jumphost-key|--jkey|--jk|-K)
        SSH_JUMPHOST_KEY="$2"
        shift 2
        ;;
      --sleep-interval|--sleep|-s|--interval|-i)
        SLEEP_INTERVAL="$2"
        shift 2
        ;;
      --skip-ssh-port-check|--skip-port-check)
        SKIP_SSH_PORT_CHECK=1
        shift
        ;;
      --luks-type|--type|-t)
        LUKS_TYPE="$2"
        shift 2
        ;;
      --luks-password|--password|-p)
        LUKS_PASSWORD="$2"
        shift 2
        ;;
      --remote-check|--healthcheck-remote-cmd|--remote-command|--remote-cmd|--rcmd)
        HEALTHCHECK_REMOTE_CMD="$2"
        shift 2
        ;;
      --healthcheck-host*|--hc-host*)
        HEALTHCHECK_REMOTE_HOSTNAME="$2"
        shift 2
        ;;
      --healthcheck-user*|--hc-user*)
        HEALTHCHECK_REMOTE_USERNAME="$2"
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
      --email-recipient|--email-to)
        EMAIL_RECIPIENT="$2"
        shift 2
        ;;
      --email-from|--email-sender)
        EMAIL_FROM="$2"
        shift 2
        ;;
      --email-subject)
        EMAIL_SUBJECT="$2"
        shift 2
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
  done

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

  log "LUKS rigmarole started. I'll be trying to unlock ${SSH_HOSTNAME}"

  while true
  do
    # Perform Healthcheck if required
    if [[ -n "$HEALTHCHECK_PORT" ]]
    then
      if nc -z -w 2 "$SSH_HOSTNAME" "$HEALTHCHECK_PORT"
      then
        if [[ -n "$DEBUG" ]]
        then
          log "Healthcheck result OK" >&2
        fi
        sleep "$SLEEP_INTERVAL"
        continue
      fi
    fi

    if [[ -n "$HEALTHCHECK_REMOTE_CMD" ]]
    then
      if SSH_HOSTNAME=${HEALTHCHECK_REMOTE_HOSTNAME:-$SSH_HOSTNAME} \
         SSH_USERNAME=${HEALTHCHECK_REMOTE_USERNAME:-$SSH_USERNAME} \
        _ssh sh -c "$HEALTHCHECK_REMOTE_CMD"
      then
        if [[ -n "$DEBUG" ]]
        then
          log "Healthcheck (remote cmd) result OK" >&2
        fi
        sleep "$SLEEP_INTERVAL"
        continue
      fi
    fi

    if [[ -z "$SKIP_SSH_PORT_CHECK" ]] && ! check_ssh_port
    then
      log "$SSH_HOSTNAME is not reachable on port $SSH_PORT" >&2
    else
      log "Trying to unlock remotely ${SSH_HOSTNAME}"

      if luks_unlock
      then
        log-notify -s "LUKS unlocked host at $SSH_HOSTNAME"
      else
        log-notify -f "Failed to unlock $SSH_HOSTNAME" >&2
      fi
    fi

    sleep "$SLEEP_INTERVAL"
  done
fi
