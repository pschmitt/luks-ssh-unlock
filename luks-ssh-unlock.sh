#!/usr/bin/env bash

SSH_HOSTNAME="${SSH_HOSTNAME:-example.com}"
SSH_CONNECT_ADDRESS="${SSH_CONNECT_ADDRESS:-}"
SSH_HOSTKEY_ALIAS="${SSH_HOSTKEY_ALIAS:-}"
SSH_KEY="${SSH_KEY:-/run/secrets/ssh_key}"
SSH_PORT="${SSH_PORT:-22}"
SSH_USERNAME="${SSH_USERNAME:-root}"
SSH_CONNECTION_TIMEOUT="${SSH_CONNECTION_TIMEOUT:-5}"
SSH_KNOWN_HOSTS="${SSH_KNOWN_HOSTS:-}"
SSH_KNOWN_HOSTS_FILE="${SSH_KNOWN_HOSTS_FILE:-}"
SSH_INITRD_KNOWN_HOSTS="${SSH_INITRD_KNOWN_HOSTS:-}"
SSH_INITRD_KNOWN_HOSTS_FILE="${SSH_INITRD_KNOWN_HOSTS_FILE:-}"

FORCE_IPV4="${FORCE_IPV4:-}"
FORCE_IPV6="${FORCE_IPV6:-}"

SSH_JUMPHOST="${SSH_JUMPHOST:-}"
SSH_JUMPHOST_USERNAME="${SSH_JUMPHOST_USERNAME:-root}"
SSH_JUMPHOST_PORT="${SSH_JUMPHOST_PORT:-${SSH_PORT}}"
SSH_JUMPHOST_KEY="${SSH_JUMPHOST_KEY:-${SSH_KEY}}"

LUKS_PASSPHRASE="${LUKS_PASSPHRASE:-}"
LUKS_PASSPHRASE_FILE="${LUKS_PASSPHRASE_FILE=-/run/secrets/luks_password_${SSH_HOSTNAME}}"
if [[ -n "${CREDENTIALS_DIRECTORY:-}" && -r "${CREDENTIALS_DIRECTORY}/luks-passphrase" ]]
then
  LUKS_PASSPHRASE_FILE="${CREDENTIALS_DIRECTORY}/luks-passphrase"
fi
LUKS_TYPE="${LUKS_TYPE:-raw}"

DEBUG="${DEBUG:-}"
RUN_ONCE="${RUN_ONCE:-}"
ACTION=run
FORCE_COLOR="${FORCE_COLOR:-}"
FORCE="${FORCE:-}"
PARANOID="${PARANOID:-}"
EVENTS_FILE="${EVENTS_FILE:-}"
SKIP_SSH_PORT_CHECK="${SKIP_SSH_PORT_CHECK:-}"
SLEEP_INTERVAL="${SLEEP_INTERVAL:-10}"
TICK_TIMEOUT="${TICK_TIMEOUT:-120}"
TMPDIR="${TMPDIR:-/tmp}"

HEALTHCHECK_PORT="${HEALTHCHECK_PORT:-}"
HEALTHCHECK_REMOTE_CMD="${HEALTHCHECK_REMOTE_CMD:-}"
HEALTHCHECK_REMOTE_HOSTNAME="${HEALTHCHECK_REMOTE_HOSTNAME:-}"
HEALTHCHECK_REMOTE_USERNAME="${HEALTHCHECK_REMOTE_USERNAME:-}"
SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE="${SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE:-default}"

INITRD_CHECKSUM_FILE="${INITRD_CHECKSUM_FILE:-}"
INITRD_CHECKSUM_DIR="${INITRD_CHECKSUM_DIR:-/dev/null}"
INITRD_CHECKSUM_SCRIPT="${INITRD_CHECKSUM_SCRIPT:-/app/bin/initrd-checksum}"
INITRD_CHECKSUM_REQUIRE_SIGNATURE="${INITRD_CHECKSUM_REQUIRE_SIGNATURE:-}"

APPRISE_TAG="${APPRISE_TAG:-}"
APPRISE_TITLE="${APPRISE_TITLE:-}"
APPRISE_URL="${APPRISE_URL:-}"

EMAIL_FROM="${EMAIL_FROM:-}"
EMAIL_RECIPIENT="${EMAIL_RECIPIENT:-}"
EMAIL_SUBJECT="${EMAIL_SUBJECT:-}"
MSMTP_ACCOUNT="${MSMTP_ACCOUNT:-}"

usage() {
  echo "Usage: $(basename "$0") [COMMAND] [OPTIONS]"
  echo
  echo "Commands:"
  echo "  run       Run the unlock service (default)"
  echo "  status    Show target config and remote boot/unlock status"
  echo
  echo "Options:"
  echo
  echo "  --help, -h     Display this help message"
  echo "  --debug, -D    Enable debug mode"
  echo "                 Env var: DEBUG"
  echo "  --once         Run one unlock attempt and exit"
  echo "  --force, -f    Skip initrd checksum and SSH host-key checks (requires --once)"
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
  echo "  --ssh-known-hosts HOSTS"
  echo "                 Expected known_hosts content to enforce server host keys"
  echo "                 Env var: SSH_KNOWN_HOSTS"
  echo "  --ssh-known-hosts-file FILE"
  echo "                 Known hosts file to enforce server host keys"
  echo "                 Env var: SSH_KNOWN_HOSTS_FILE"
  echo "  --ssh-initrd-known-hosts HOSTS"
  echo "                 Expected known_hosts content for unlock (initrd) SSH host keys only"
  echo "                 Env var: SSH_INITRD_KNOWN_HOSTS"
  echo "  --ssh-initrd-known-hosts-file FILE"
  echo "                 Known hosts file for unlock (initrd) SSH host keys only"
  echo "                 Env var: SSH_INITRD_KNOWN_HOSTS_FILE"
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
  echo "  --tick-timeout, --timeout SECONDS"
  echo "                 Timeout for each tick (default: 120)"
  echo "                 Env var: TICK_TIMEOUT"
  echo "  --skip-ssh-port-check, --skip-port-check"
  echo "                 Skip SSH port check"
  echo "                 Env var: SKIP_SSH_PORT_CHECK"
  echo

  echo "  --luks-type, --type, -t TYPE"
  echo "                 LUKS type to use (raw, systemd, systemd-tool, luks-mount)"
  echo "                 Env var: LUKS_TYPE"
  echo "  --luks-passphrase, --luks-password, --password, -p PASSWORD"
  echo "                 LUKS password to use (insecure: visible in ps; prefer -F)"
  echo "                 Env var: LUKS_PASSPHRASE"
  echo "  --luks-passphrase-file, --luks-password-file, -F FILE"
  echo "                 LUKS password file to use"
  echo "                 Env var: LUKS_PASSPHRASE_FILE"
  echo "  --initrd-checksum-file FILE"
  echo "                 Expected initrd checksum file to validate before unlocking"
  echo "                 Env var: INITRD_CHECKSUM_FILE"
  echo "  --initrd-checksum-dir DIR"
  echo "                 Directory to store fetched initrd checksum snapshots"
  echo "                 Env var: INITRD_CHECKSUM_DIR"
  echo "  PARANOID=1     Run initrd-checksum validation in paranoid mode (--paranoid)"
  echo "                 Env var: PARANOID"
  echo "  INITRD_CHECKSUM_REQUIRE_SIGNATURE=1"
  echo "                 Fail closed if the fetched checksum baseline has no valid"
  echo "                 ssh-keygen signature (baseline.sig) from the host's SSH key"
  echo "                 Env var: INITRD_CHECKSUM_REQUIRE_SIGNATURE"
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
  echo "  --healthcheck-known-hosts-type TYPE"
  echo "                 Which known_hosts set to use for healthcheck SSH (default|initrd)"
  echo "                 Env var: SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE"
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
  echo

  echo "  --msmtp-email-account, --email-account ACCOUNT"
  echo "                 msmtp account to use for sending emails"
  echo "                 Env var: MSMTP_ACCOUNT"
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

# Usage: log [-d|-i|-w|-e] MESSAGE...
# Under systemd, journald already timestamps every line, so we emit a syslog
# priority prefix instead (journalctl then colors/filters by level).
log() {
  local level=info

  case "$1" in
    -d|--debug)
      level=debug
      shift
      ;;
    -i|--info)
      shift
      ;;
    -w|--warning)
      level=warning
      shift
      ;;
    -e|--error)
      level=error
      shift
      ;;
  esac

  if [[ "$level" == debug && -z "$DEBUG" ]]
  then
    return 0
  fi

  local prefix=''
  local label=''

  case "$level" in
    debug)
      prefix='<7>'
      label='debug: '
      ;;
    warning)
      prefix='<4>'
      label='warning: '
      ;;
    error)
      prefix='<3>'
      label='error: '
      ;;
    *)
      prefix='<6>'
      ;;
  esac

  if [[ -z "${JOURNAL_STREAM:-}" ]]
  then
    prefix="$(date -Iseconds) "
  fi

  printf '%s%s%s\n' "$prefix" "$label" "$*" >&2
}

_state_file() {
  printf '%s/luks-ssh-unlock-%s.state' "${TMPDIR%/}" "$SSH_HOSTNAME"
}

previous_state() {
  local state_file
  state_file=$(_state_file)

  if [[ -r "$state_file" ]]
  then
    printf '%s' "$(< "$state_file")"
  fi
}

# Usage: log_state [--notify] STATE LEVEL MESSAGE
# Log MESSAGE only when the target's state differs from the previous tick, so
# a steady state (booted, unreachable, ...) is reported once instead of every
# SLEEP_INTERVAL. Ticks run in a subshell, hence the state file.
log_state() {
  local notify=''

  if [[ "$1" == --notify ]]
  then
    notify=1
    shift
  fi

  local state="$1"
  local level="$2"
  local message="$3"

  if [[ "$state" == "$(previous_state)" && -z "$RUN_ONCE" ]]
  then
    log -d "${message} (unchanged)"
    return 1
  fi

  printf '%s\n' "$state" > "$(_state_file)"

  if [[ -n "$notify" ]]
  then
    log-notify "--${level}" "$message"
  else
    log "--${level}" "$message"
  fi
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
  msg=${msg//#luks_password/[REDACTED]}

  msg=${msg//#remote_cmd/${HEALTHCHECK_REMOTE_CMD}}
  msg=${msg//#remote_hostname/${HEALTHCHECK_REMOTE_HOSTNAME}}
  msg=${msg//#remote_username/${HEALTHCHECK_REMOTE_USERNAME}}

  echo -n "$msg"
}

log-notify() {
  local event_type

  if [[ "$ACTION" == status ]]
  then
    return 0
  fi

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
  case "$event_type" in
    warning)
      log -w "$event"
      ;;
    failure)
      log -e "$event"
      ;;
    *)
      log "$event"
      ;;
  esac

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
        printenv | redact_env
      fi

    } | {
      if [[ -n "$MSMTP_ACCOUNT" ]]
      then
        msmtp -a "$MSMTP_ACCOUNT" "$EMAIL_RECIPIENT"
      else
        sendmail "$EMAIL_RECIPIENT"
      fi
    }
  fi
}

_known_hosts_path() {
  local type="${1:-default}"
  local hosts_var
  local file_var
  local tmp_suffix

  if [[ -n "$FORCE" && -n "$RUN_ONCE" ]]
  then
    if [[ -n "$DEBUG" ]]
    then
      log "FORCE: bypassing SSH host-key verification for ${SSH_HOSTNAME}"
    fi
    echo /dev/null
    return 0
  fi

  case "$type" in
    initrd)
      hosts_var="SSH_INITRD_KNOWN_HOSTS"
      file_var="SSH_INITRD_KNOWN_HOSTS_FILE"
      tmp_suffix="ssh_initrd_known_hosts"
      ;;
    *)
      hosts_var="SSH_KNOWN_HOSTS"
      file_var="SSH_KNOWN_HOSTS_FILE"
      tmp_suffix="ssh_known_hosts"
      ;;
  esac

  local inline_hosts="${!hosts_var}"

  if [[ -n "$inline_hosts" ]]
  then
    local known_hosts_path="${TMPDIR%/}/${tmp_suffix}"
    printf "%s\n" "$inline_hosts" > "$known_hosts_path"
    chmod 600 "$known_hosts_path"
    if [[ -n "$DEBUG" ]]
    then
      log "Using known_hosts (type: $type, source: inline) at $known_hosts_path"
    fi
    echo "$known_hosts_path"
    return 0
  fi

  local known_hosts_file="${!file_var}"

  if [[ -n "$known_hosts_file" ]]
  then
    if [[ ! -r "$known_hosts_file" ]]
    then
      echo "${file_var} at $known_hosts_file is not readable." >&2
      return 2
    fi

    if [[ -n "$DEBUG" ]]
    then
      log "Using known_hosts (type: $type, source: file) at $known_hosts_file"
    fi
    echo "$known_hosts_file"
    return 0
  fi

  if [[ -n "$DEBUG" ]]
  then
    log "Using known_hosts (type: $type, source: none) at /dev/null"
  fi

  echo /dev/null
}

_ssh() {
  local ssh_opts=(-o ControlMaster=no)
  local known_hosts_type="${SSH_KNOWN_HOSTS_TYPE_OVERRIDE:-default}"

  local known_hosts_file
  known_hosts_file=$(_known_hosts_path "$known_hosts_type") || return 2

  if [[ -n "$FORCE" && -n "$RUN_ONCE" && "$known_hosts_type" == initrd ]]
  then
    known_hosts_file=/dev/null
  fi

  if [[ -z "$known_hosts_file" ]]
  then
    known_hosts_file=/dev/null
  fi

  ssh_opts+=(-o "UserKnownHostsFile=${known_hosts_file}")
  if [[ -n "$SSH_CONNECT_ADDRESS" ]]
  then
    ssh_opts+=(-o "HostName=${SSH_CONNECT_ADDRESS}")
  fi
  if [[ -n "$SSH_HOSTKEY_ALIAS" ]]
  then
    ssh_opts+=(-o "HostKeyAlias=${SSH_HOSTKEY_ALIAS}")
  fi

  if [[ "$known_hosts_file" == /dev/null ]]
  then
    ssh_opts+=(-o StrictHostKeyChecking=no)
    if [[ -n "$FORCE" && -n "$RUN_ONCE" ]]
    then
      ssh_opts+=(-o GlobalKnownHostsFile=/dev/null)
    fi
  else
    ssh_opts+=(-o StrictHostKeyChecking=yes)
  fi

  if [[ -n "$FORCE_IPV4" ]]
  then
    ssh_opts+=(-4)
  elif [[ -n "$FORCE_IPV6" ]]
  then
    ssh_opts+=(-6)
  fi

  local extra_args=()
  if [[ -n "$SSH_JUMPHOST" ]]
  then
    # We can't use JumpHost here since it does not inherit the
    # StrictHostKeyChecking settings etc. Target-only options (HostName,
    # HostKeyAlias) must not leak into the jumphost connection.
    extra_args=(-o "$(_jumphost_proxy_command "$known_hosts_file")")
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

_ssh_remote_command() {
  local remote_command="$1"
  local quoted_command

  printf -v quoted_command '%q' "$remote_command"
  _ssh "sh -c $quoted_command"
}

# Print a ProxyCommand ssh option value that reaches the target through
# SSH_JUMPHOST, verifying the jumphost against KNOWN_HOSTS_FILE.
_jumphost_proxy_command() {
  local known_hosts_file="$1"
  local proxy_opts=(-F /dev/null -o ConnectTimeout="$SSH_CONNECTION_TIMEOUT")
  proxy_opts+=(-o "UserKnownHostsFile=${known_hosts_file}")
  if [[ "$known_hosts_file" == /dev/null ]]
  then
    proxy_opts+=(-o StrictHostKeyChecking=no)
    if [[ -n "$FORCE" && -n "$RUN_ONCE" ]]
    then
      proxy_opts+=(-o GlobalKnownHostsFile=/dev/null)
    fi
  else
    proxy_opts+=(-o StrictHostKeyChecking=yes)
  fi
  if [[ -n "$FORCE_IPV4" ]]
  then
    proxy_opts+=(-4)
  elif [[ -n "$FORCE_IPV6" ]]
  then
    proxy_opts+=(-6)
  fi
  proxy_opts+=(-p "$SSH_JUMPHOST_PORT" -i "$SSH_JUMPHOST_KEY" -l "$SSH_JUMPHOST_USERNAME" "$SSH_JUMPHOST" -W %h:%p)
  local proxy_cmd
  printf -v proxy_cmd '%q ' "${proxy_opts[@]}"
  printf 'ProxyCommand=ssh %s\n' "${proxy_cmd% }"
}

_scp() {
  local scp_opts=(-o ControlMaster=no)
  local known_hosts_type="${SSH_KNOWN_HOSTS_TYPE_OVERRIDE:-default}"

  local known_hosts_file
  known_hosts_file=$(_known_hosts_path "$known_hosts_type") || return 2

  if [[ -z "$known_hosts_file" ]]
  then
    known_hosts_file=/dev/null
  fi

  scp_opts+=(-o "UserKnownHostsFile=${known_hosts_file}")
  if [[ -n "$SSH_CONNECT_ADDRESS" ]]
  then
    scp_opts+=(-o "HostName=${SSH_CONNECT_ADDRESS}")
  fi
  if [[ -n "$SSH_HOSTKEY_ALIAS" ]]
  then
    scp_opts+=(-o "HostKeyAlias=${SSH_HOSTKEY_ALIAS}")
  fi

  if [[ "$known_hosts_file" == /dev/null ]]
  then
    scp_opts+=(-o StrictHostKeyChecking=no)
    if [[ -n "$FORCE" && -n "$RUN_ONCE" ]]
    then
      scp_opts+=(-o GlobalKnownHostsFile=/dev/null)
    fi
  else
    scp_opts+=(-o StrictHostKeyChecking=yes)
  fi

  if [[ -n "$FORCE_IPV4" ]]
  then
    scp_opts+=(-4)
  elif [[ -n "$FORCE_IPV6" ]]
  then
    scp_opts+=(-6)
  fi

  if [[ -n "$SSH_PORT" ]]
  then
    scp_opts+=(-P "$SSH_PORT")
  fi

  local extra_args=()
  if [[ -n "$SSH_JUMPHOST" ]]
  then
    extra_args=(-o "$(_jumphost_proxy_command "$known_hosts_file")")
  fi

  scp -F /dev/null \
    -o ConnectTimeout="$SSH_CONNECTION_TIMEOUT" \
    "${scp_opts[@]}" \
    -i "$SSH_KEY" \
    "${extra_args[@]}" \
    "$@"
}

_ssh_jumphost() {
  local ssh_opts=(-o ControlMaster=no)
  local known_hosts_type="${SSH_KNOWN_HOSTS_TYPE_OVERRIDE:-default}"

  local known_hosts_file
  known_hosts_file=$(_known_hosts_path "$known_hosts_type") || return 2

  if [[ -z "$known_hosts_file" ]]
  then
    known_hosts_file=/dev/null
  fi

  ssh_opts+=(-o "UserKnownHostsFile=${known_hosts_file}")

  if [[ "$known_hosts_file" == /dev/null ]]
  then
    ssh_opts+=(-o StrictHostKeyChecking=no)
    if [[ -n "$FORCE" && -n "$RUN_ONCE" ]]
    then
      ssh_opts+=(-o GlobalKnownHostsFile=/dev/null)
    fi
  else
    ssh_opts+=(-o StrictHostKeyChecking=yes)
  fi

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
  if [[ -n "$SSH_CONNECT_ADDRESS" ]]
  then
    echo "$SSH_CONNECT_ADDRESS"
    return 0
  fi

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
  log -d "Resolved $SSH_HOSTNAME to $resolved_hostname"

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    _ssh_jumphost nc "$resolved_hostname" "$SSH_PORT" <<< "" 2>&1 | \
      grep -qiE "^SSH-"
    return "$?"
  fi

  nc -z -w 2 "${SSH_CONNECT_ADDRESS:-$resolved_hostname}" "$SSH_PORT"
}

# With FORCE_IPV4/FORCE_IPV6, resolve the target once per tick and connect to
# that address, keeping the hostname for host-key checks. Otherwise a jumphost
# resolves "-W host:port" itself and may try stale AAAA records first (each
# costing a connect timeout), regardless of our -4/-6.
pin_connect_address() {
  if [[ -n "$SSH_CONNECT_ADDRESS" ]] || is-an-ip-address "$SSH_HOSTNAME"
  then
    return 0
  fi

  if [[ -z "$FORCE_IPV4" && -z "$FORCE_IPV6" ]]
  then
    return 0
  fi

  local address
  address=$(resolve-hostname)

  if ! is-an-ip-address "$address"
  then
    log -d "Could not resolve ${SSH_HOSTNAME}; connecting by name"
    return 0
  fi

  SSH_CONNECT_ADDRESS="$address"
  SSH_HOSTKEY_ALIAS="${SSH_HOSTKEY_ALIAS:-$SSH_HOSTNAME}"
  log -d "Connecting to ${SSH_HOSTNAME} at ${SSH_CONNECT_ADDRESS}"
}

# Return 0 if the target is in its initrd, 1 if it definitely is not, and 2
# if we could not tell because SSH itself failed (bad key, host key mismatch,
# jumphost down, ...). The SSH error is stored in INITRD_SSH_ERROR.
is_initrd() {
  local output
  local rc=0

  INITRD_SSH_ERROR=''
  output=$(
    SSH_KNOWN_HOSTS_TYPE_OVERRIDE=initrd \
      _ssh test -e /etc/initrd-release 2>&1
  ) || rc=$?

  case "$rc" in
    0)
      return 0
      ;;
    1)
      return 1
      ;;
  esac

  INITRD_SSH_ERROR=$(
    grep -v '^Warning: Permanently added' <<< "$output" | \
      grep -v '^[[:space:]]*$' | \
      tail -n 1
  )
  INITRD_SSH_ERROR="${INITRD_SSH_ERROR:-ssh exited with code ${rc}}"
  return 2
}

verify_initrd_checksum_signature() {
  local sig_file="${INITRD_CHECKSUM_FILE}.sig"

  local allowed_signers
  allowed_signers=$(_known_hosts_path default) || return 1

  if [[ "$allowed_signers" == /dev/null ]]
  then
    if [[ -n "$INITRD_CHECKSUM_REQUIRE_SIGNATURE" ]]
    then
      log-notify -w "No known_hosts configured for ${SSH_HOSTNAME}; cannot verify initrd checksum signature (required)"
      return 1
    fi

    log-notify -w "No known_hosts configured for ${SSH_HOSTNAME}; skipping initrd checksum signature verification"
    return 0
  fi

  if [[ ! -r "$sig_file" ]]
  then
    if [[ -n "$INITRD_CHECKSUM_REQUIRE_SIGNATURE" ]]
    then
      log-notify -w "Initrd checksum signature ${sig_file} is missing or not readable (required)"
      return 1
    fi

    log-notify -w "Initrd checksum signature ${sig_file} is missing or not readable; skipping signature verification"
    return 0
  fi

  if ! ssh-keygen -Y verify \
    -f "$allowed_signers" \
    -I "$SSH_HOSTNAME" \
    -n initrd-checksum \
    -s "$sig_file" \
    < "$INITRD_CHECKSUM_FILE" > /dev/null
  then
    log-notify -w "Initrd checksum signature verification FAILED for ${SSH_HOSTNAME}; baseline may be tampered with"
    return 1
  fi

  if [[ -n "$DEBUG" ]]
  then
    log "Initrd checksum signature verified for ${SSH_HOSTNAME}"
  fi

  return 0
}

check_initrd_checksum() {
  if [[ -z "$INITRD_CHECKSUM_FILE" ]]
  then
    return 0
  fi

  local checksum_script="${INITRD_CHECKSUM_SCRIPT:-/app/bin/initrd-checksum}"
  if [[ ! -x "$checksum_script" ]]
  then
    log-notify -w "Initrd checksum validation requested but ${checksum_script} is missing or not executable"
    return 1
  fi

  if [[ ! -r "$INITRD_CHECKSUM_FILE" ]]
  then
    log-notify -w "Initrd checksum file ${INITRD_CHECKSUM_FILE} is not readable"
    return 1
  fi

  if ! verify_initrd_checksum_signature
  then
    log-notify -w "Skipping unlock attempt for ${SSH_HOSTNAME} due to initrd checksum signature failure"
    return 1
  fi

  local known_hosts_file
  known_hosts_file=$(_known_hosts_path initrd) || return 1

  local checksum_args=(
    checksum
    --quiet
    --host "$SSH_HOSTNAME"
    --ssh-user "$SSH_USERNAME"
    --identity "$SSH_KEY"
    --diff "$INITRD_CHECKSUM_FILE"
  )
  if [[ "$known_hosts_file" == /dev/null ]]
  then
    checksum_args+=(--insecure-ssh)
  elif [[ -n "$known_hosts_file" ]]
  then
    checksum_args+=(--known-hosts-file "$known_hosts_file")
  fi

  if [[ -n "$SSH_CONNECT_ADDRESS" ]]
  then
    checksum_args+=(--ssh-option "HostName=${SSH_CONNECT_ADDRESS}")
  fi
  if [[ -n "$SSH_HOSTKEY_ALIAS" ]]
  then
    checksum_args+=(--ssh-option "HostKeyAlias=${SSH_HOSTKEY_ALIAS}")
  fi

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    local default_known_hosts_file
    default_known_hosts_file=$(_known_hosts_path default) || return 1
    checksum_args+=(
      --ssh-option "$(_jumphost_proxy_command "${default_known_hosts_file:-/dev/null}")"
    )
  fi

  if [[ -n "$PARANOID" ]]
  then
    checksum_args+=(--paranoid)
  fi

  if [[ -n "$DEBUG" ]]
  then
    checksum_args+=(--debug)
  fi

  if [[ -n "$DEBUG" ]]
  then
    log "Running initrd checksum validation via ${checksum_script}"
  fi

  local checksum_output
  if checksum_output=$("$checksum_script" "${checksum_args[@]}" 2>&1)
  then
    if [[ -n "$DEBUG" && -n "$checksum_output" ]]
    then
      log "$checksum_output"
    fi
    return 0
  fi

  if [[ -n "$DEBUG" && -n "$checksum_output" ]]
  then
    log "$checksum_output"
  fi

  log-notify -w "Initrd checksum validation failed for ${SSH_HOSTNAME}; skipping unlock attempt"
  return 1
}

show_status() {
  local reset='' green='' yellow='' red='' cyan='' blue='' magenta='' dim=''
  local key_check='not configured' signature_check='optional'
  local key_color='' signature_color=''

  if [[ -n "$FORCE_COLOR" || ( -t 1 && "${TERM:-}" != dumb ) ]]
  then
    reset=$'\033[0m'
    green=$'\033[32m'
    yellow=$'\033[33m'
    red=$'\033[31m'
    cyan=$'\033[36m'
    blue=$'\033[34m'
    magenta=$'\033[35m'
    dim=$'\033[2m'
  fi

  key_color=$yellow
  signature_color=$yellow
  if [[ -n "$INITRD_CHECKSUM_REQUIRE_SIGNATURE" ]]
  then
    signature_check=required
    signature_color=$green
  fi

  if [[ -n "$SSH_KNOWN_HOSTS_FILE" || -n "$SSH_KNOWN_HOSTS" ]] &&
    [[ -n "$SSH_INITRD_KNOWN_HOSTS_FILE" || -n "$SSH_INITRD_KNOWN_HOSTS" ]]
  then
    key_check='regular and initrd keys configured'
    key_color=$green
  fi

  printf '%sLUKS SSH unlock: %s%s\n' "$cyan" "$SSH_HOSTNAME" "$reset"
  printf '  %-19s %s%s@%s:%s%s\n' 'SSH target' "$cyan" "$SSH_USERNAME" "$SSH_HOSTNAME" "$SSH_PORT" "$reset"
  printf '  %-19s %s%s%s\n' 'LUKS type' "$magenta" "$LUKS_TYPE" "$reset"
  printf '  %-19s %s%s%s\n' 'Healthcheck' "$blue" "${HEALTHCHECK_REMOTE_CMD:-not configured}" "$reset"
  printf '  %-19s %s%s%s\n' 'Initrd checksum' "$cyan" "${INITRD_CHECKSUM_FILE:-not configured}" "$reset"
  printf '  %-19s %s%s%s\n' 'Signature check' "$signature_color" "$signature_check" "$reset"
  printf '  %-19s %s%s%s\n' 'Host-key checks' "$key_color" "$key_check" "$reset"
  if [[ -n "$INITRD_CHECKSUM_FILE" && -r "$INITRD_CHECKSUM_FILE" ]]
  then
    printf '  %-19s %s%s%s\n' 'Baseline refreshed' "$yellow" "$(stat -c '%y' "$INITRD_CHECKSUM_FILE")" "$reset"
  else
    printf '  %-19s %s%s%s\n' 'Baseline refreshed' "$yellow" 'not available' "$reset"
  fi
  printf '\n%sTarget state%s\n' "$cyan" "$reset"

  pin_connect_address

  if [[ -n "$HEALTHCHECK_PORT" ]] && nc -z -w 2 "${SSH_CONNECT_ADDRESS:-$SSH_HOSTNAME}" "$HEALTHCHECK_PORT"
  then
    show_remote_metadata default "$cyan" "$yellow" "$reset"
    printf '  %s✅ Unlocked; healthcheck port %s is open%s\n' "$green" "$HEALTHCHECK_PORT" "$reset"
    return 0
  fi

  if [[ -n "$HEALTHCHECK_REMOTE_CMD" ]]
  then
    if SSH_HOSTNAME=${HEALTHCHECK_REMOTE_HOSTNAME:-$SSH_HOSTNAME} \
      SSH_USERNAME=${HEALTHCHECK_REMOTE_USERNAME:-$SSH_USERNAME} \
      SSH_KNOWN_HOSTS_TYPE_OVERRIDE=${SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE:-default} \
      _ssh_remote_command "$HEALTHCHECK_REMOTE_CMD" >/dev/null 2>&1
    then
      show_remote_metadata "${SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE:-default}" "$cyan" "$yellow" "$reset"
      printf '  %s✅ Unlocked; normal-boot healthcheck passed%s\n' "$green" "$reset"
      return 0
    fi
    printf '  %s⚠️  Normal-boot healthcheck failed%s\n' "$yellow" "$reset"
  else
    printf '  %s⚠️  No normal-boot healthcheck configured%s\n' "$yellow" "$reset"
  fi

  if SSH_KNOWN_HOSTS_TYPE_OVERRIDE=initrd _ssh true >/dev/null 2>&1
  then
    show_remote_metadata initrd "$cyan" "$yellow" "$reset"
    printf '  %s✅ Initrd SSH is reachable%s\n' "$yellow" "$reset"
    local initrd_rc=0
    is_initrd || initrd_rc=$?
    if [[ "$initrd_rc" -eq 1 ]]
    then
      printf '  %s⚠️  /etc/initrd-release is absent; target is not in initrd%s\n' "$yellow" "$reset"
      return 1
    elif [[ "$initrd_rc" -ne 0 ]]
    then
      printf '  %s❌ Initrd check failed: %s%s\n' "$red" "$INITRD_SSH_ERROR" "$reset"
      return 1
    fi
    printf '  %s✅ Initrd environment detected%s\n' "$green" "$reset"
    if [[ -n "$INITRD_CHECKSUM_FILE" ]]
    then
      local checksum_output
      if checksum_output=$(check_initrd_checksum 2>&1)
      then
        printf '  %s✅ Initrd checksum/signature validated%s\n' "$green" "$reset"
        if [[ -n "$DEBUG" && -n "$checksum_output" ]]
        then
          printf '%s\n' "$checksum_output" >&2
        fi
      else
        printf '  %s❌ Initrd checksum/signature validation failed%s\n' "$red" "$reset"
        if [[ -n "$DEBUG" && -n "$checksum_output" ]]
        then
          printf '%s\n' "$checksum_output" >&2
        fi
        return 1
      fi
    else
      printf '  %s⚠️  Initrd checksum validation is not configured%s\n' "$yellow" "$reset"
    fi
    return 0
  fi

  printf '  %s❌ Target is not reachable through normal or initrd SSH%s\n' "$red" "$reset"
  printf '%s%s%s\n' "$dim" 'Check network reachability and SSH credentials.' "$reset"
  return 1
}

show_remote_metadata() {
  local known_hosts_type="$1"
  local value_color="${2:-}" date_color="${3:-}" reset="${4:-}"
  local target_uptime='unavailable'
  local target_checksum_time='unavailable'

  if target_uptime=$(
    SSH_KNOWN_HOSTS_TYPE_OVERRIDE="$known_hosts_type" _ssh uptime -p 2>/dev/null
  )
  then
    :
  else
    target_uptime='unavailable'
  fi

  if target_checksum_time=$(
    SSH_KNOWN_HOSTS_TYPE_OVERRIDE="$known_hosts_type" \
      _ssh stat -c '%y' /etc/initrd-checksum/checksum 2>/dev/null
  )
  then
    :
  else
    target_checksum_time='unavailable'
  fi

  printf '  %-19s %s%s%s\n' 'Target uptime' "$value_color" "$target_uptime" "$reset"
  printf '  %-19s %s%s%s\n' 'Checksum date' "$date_color" "$target_checksum_time" "$reset"
}

fetch_initrd_checksum() {
  if [[ -z "$INITRD_CHECKSUM_DIR" || "$INITRD_CHECKSUM_DIR" == "/dev/null" ]]
  then
    return 0
  fi

  local remote_checksum_path="/etc/initrd-checksum"
  local checksum_dir="${INITRD_CHECKSUM_DIR%/}/${SSH_HOSTNAME}"
  local checksum_file="${checksum_dir}/initrd-checksum/checksum"
  local old_checksum=''
  local new_checksum=''

  if [[ -r "$checksum_file" ]]
  then
    old_checksum=$(sha256sum "$checksum_file" | cut -d ' ' -f 1)
  fi

  if ! mkdir -p "$checksum_dir"
  then
    log-notify -w "Failed to create initrd checksum directory at ${checksum_dir}"
    return 1
  fi

  if ! _scp -r "${SSH_USERNAME}@${SSH_HOSTNAME}:${remote_checksum_path}/" "${checksum_dir}/"
  then
    log-notify -w "Failed to fetch ${remote_checksum_path} from ${SSH_HOSTNAME}"
    return 1
  fi

  if [[ -r "$checksum_file" ]]
  then
    new_checksum=$(sha256sum "$checksum_file" | cut -d ' ' -f 1)
  fi

  if [[ "$old_checksum" != "$new_checksum" ]]
  then
    log "Updated the stored initrd checksum baseline of ${SSH_HOSTNAME}"
  elif [[ -n "$DEBUG" ]]
  then
    log "Initrd checksum baseline unchanged for ${SSH_HOSTNAME}"
  fi

  if [[ -n "$DEBUG" ]]
  then
    log "Stored initrd checksum from ${SSH_HOSTNAME} to ${checksum_dir}"
  fi

  return 0
}

# Replace the LUKS passphrase in stdin, and strip terminal noise (ANSI
# escapes, backspaces and other control characters, password mask bullets)
# that would otherwise end up in the journal as binary blobs or reveal the
# passphrase length.
sanitize_output() {
  local line

  while IFS= read -r line || [[ -n "$line" ]]
  do
    if [[ -n "$LUKS_PASSPHRASE" ]]
    then
      line=${line//"$LUKS_PASSPHRASE"/[REDACTED]}
    fi
    printf '%s\n' "$line"
  done | LC_ALL=C sed -E \
    -e 's/\x1b\[[0-9;?]*[A-Za-z]//g' \
    -e 's/[\x01-\x08\x0b-\x1f\x7f]//g' \
    -e 's/(\xe2\x80\xa2|\*){2,}//g' \
    -e 's/[[:space:]]+$//' | \
    grep -v '^$'
}

# Redact values of secret-looking environment variables (debug emails).
redact_env() {
  sed -E 's/^([^=]*(PASS|SECRET|TOKEN|KEY|CREDENTIAL)[^=]*)=.*/\1=[REDACTED]/I' | \
    sanitize_output
}

# Log remote unlock output (debug level only) and remember its last line so
# a failure can be reported with context. Never logs the passphrase.
UNLOCK_OUTPUT_TAIL=''
log_unlock_output() {
  local output
  output=$(sanitize_output <<< "$1")

  if [[ -z "$output" ]]
  then
    return 0
  fi

  UNLOCK_OUTPUT_TAIL=$(tail -n 1 <<< "$output")

  local line
  while IFS= read -r line
  do
    log -d "${SSH_HOSTNAME}: ${line}"
  done <<< "$output"
}

# Run CMD... with the passphrase on stdin; output goes through
# log_unlock_output instead of straight to the journal.
run_with_passphrase() {
  local output
  local rc=0

  output=$("$@" 2>&1 <<< "$LUKS_PASSPHRASE") || rc=$?
  log_unlock_output "$output"
  return "$rc"
}

systemd-tty-unlock() {
  local ready_marker="LUKS-SSH-UNLOCK-READY-${BASHPID}-${RANDOM}"
  local remote_command="stty -echo && printf '\\n%s\\n' '${ready_marker}' && exec systemd-tty-ask-password-agent"
  local line
  local ssh_status=0
  local ready=0

  coproc ask_password_agent {
    _ssh -o LogLevel=ERROR -tt "$remote_command" 2>&1
  }

  local ssh_pid="$!"
  local output_fd="${ask_password_agent[0]}"
  local input_fd="${ask_password_agent[1]}"

  # Do not send the passphrase until the remote PTY has disabled echo.
  while IFS= read -r -u "$output_fd" line
  do
    line=${line%$'\r'}
    if [[ "$line" == "$ready_marker" ]]
    then
      ready=1
      break
    fi

    log_unlock_output "$line"
  done

  if [[ "$ready" -ne 1 ]]
  then
    wait "$ssh_pid" || ssh_status=$?
    log -w "Failed to prepare the remote terminal on ${SSH_HOSTNAME} for the LUKS passphrase"
    if [[ "$ssh_status" -eq 0 ]]
    then
      ssh_status=1
    fi
    return "$ssh_status"
  fi

  printf '%s\n' "$LUKS_PASSPHRASE" >&"$input_fd"

  while IFS= read -r -u "$output_fd" line
  do
    log_unlock_output "$line"
  done

  wait "$ssh_pid"
}

luks_unlock() {
  local SSH_KNOWN_HOSTS_TYPE_OVERRIDE=initrd

  case "$LUKS_TYPE" in
    raw|direct)
      run_with_passphrase _ssh
      ;;
    systemd-tool|arch)
      local disk
      local mapper

      mapper=$(_ssh \
        systemctl --no-pager list-unit-files | \
        sed -nr 's/^systemd-cryptsetup@(.+).service.*/\1/p')

      if [[ -z "$mapper" ]]
      then
        log -w "Failed to determine the root mapper name on ${SSH_HOSTNAME}"
        return 1
      fi

      disk=$(_ssh \
        systemctl cat --no-pager "systemd-cryptsetup@${mapper}" | \
        sed -nr "s;ExecStart=(.+)(/dev/disk/by-uuid/[^ '\"]+).*;\2;p")

      if [[ -z "$disk" ]]
      then
        log -w "Failed to determine the root disk path on ${SSH_HOSTNAME}"
        return 1
      fi

      if ! run_with_passphrase _ssh cryptsetup luksOpen "$disk" "$mapper" -
      then
        log -w "cryptsetup could not open ${disk} on ${SSH_HOSTNAME}"
        return 1
      fi

      local restart_output
      restart_output=$(_ssh systemctl restart "systemd-cryptsetup@${mapper}" 2>&1)
      local restart_rc=$?
      log_unlock_output "$restart_output"
      return "$restart_rc"
      ;;

    # https://github.com/gsauthof/dracut-sshd/issues/32
    systemd|dracut-systemd|dracut-sshd|dracut|alt)
      systemd-tty-unlock
      ;;

    # https://github.com/pschmitt/luks-mount.sh
    luks-mount)
      run_with_passphrase _ssh luks-mount
      ;;
  esac
}

# Describe how we reach the target, for log messages.
target_route() {
  local route="${SSH_USERNAME}@${SSH_HOSTNAME}:${SSH_PORT}"

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    route+=" via ${SSH_JUMPHOST}"
  fi

  printf '%s' "$route"
}

# Return 0 if the target is booted and unlocked.
healthcheck() {
  if [[ -n "$HEALTHCHECK_PORT" ]]
  then
    if nc -z -w 2 "${SSH_CONNECT_ADDRESS:-$SSH_HOSTNAME}" "$HEALTHCHECK_PORT"
    then
      return 0
    fi
    log -d "Healthcheck port ${HEALTHCHECK_PORT} on ${SSH_HOSTNAME} is closed"
  fi

  if [[ -z "$HEALTHCHECK_REMOTE_CMD" ]]
  then
    return 1
  fi

  local connect_address="$SSH_CONNECT_ADDRESS"
  local hostkey_alias="$SSH_HOSTKEY_ALIAS"
  if [[ -n "$HEALTHCHECK_REMOTE_HOSTNAME" && "$HEALTHCHECK_REMOTE_HOSTNAME" != "$SSH_HOSTNAME" ]]
  then
    connect_address=''
    hostkey_alias=''
  fi

  local healthcheck_output
  if healthcheck_output=$(
    SSH_CONNECT_ADDRESS="$connect_address" \
      SSH_HOSTKEY_ALIAS="$hostkey_alias" \
      SSH_HOSTNAME=${HEALTHCHECK_REMOTE_HOSTNAME:-$SSH_HOSTNAME} \
      SSH_USERNAME=${HEALTHCHECK_REMOTE_USERNAME:-$SSH_USERNAME} \
      SSH_KNOWN_HOSTS_TYPE_OVERRIDE=${SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE:-default} \
      _ssh_remote_command "$HEALTHCHECK_REMOTE_CMD" 2>&1
  )
  then
    log -d "Healthcheck output: ${healthcheck_output}"
    return 0
  fi

  log -d "Healthcheck failed for ${SSH_HOSTNAME}: ${healthcheck_output:-no output}"
  return 1
}

run_tick() {
  # One-shot invocations must also avoid trying to unlock a host that is
  # already booted. A failed healthcheck is only a signal to check the initrd;
  # /etc/initrd-release is the explicit test before any unlock attempt.
  pin_connect_address

  if healthcheck
  then
    fetch_initrd_checksum
    log_state booted info "${SSH_HOSTNAME} is up and unlocked (healthcheck passed); nothing to do"
    return 0
  fi

  if [[ -z "$SKIP_SSH_PORT_CHECK" ]] && ! check_ssh_port
  then
    log_state unreachable warning \
      "${SSH_HOSTNAME} is down: SSH on $(target_route) is not reachable. Retrying every ${SLEEP_INTERVAL}s"
    return 1
  fi

  local initrd_rc=0
  is_initrd || initrd_rc=$?

  case "$initrd_rc" in
    1)
      log_state not-initrd warning \
        "${SSH_HOSTNAME} answers on SSH but is not in its initrd (no /etc/initrd-release) and the healthcheck failed. Not unlocking; is it still booting or shutting down?"
      return 0
      ;;
    2)
      log_state --notify initrd-ssh-failed error \
        "Cannot tell whether ${SSH_HOSTNAME} is waiting for its LUKS passphrase: SSH login to $(target_route) failed: ${INITRD_SSH_ERROR}. If it is in initrd, check that this unlocker's key and host keys are set up for the initrd SSH server"
      return 1
      ;;
  esac

  # Keep a failed unlock as the current state while we retry, so a host that
  # keeps rejecting the passphrase is reported once, not on every tick.
  if [[ "$(previous_state)" != unlock-failed ]]
  then
    log_state initrd info "${SSH_HOSTNAME} is waiting in initrd for its LUKS passphrase"
  fi

  if [[ -n "$FORCE" ]]
  then
    log -w "Forced unlock: skipping initrd checksum/signature and SSH host-key checks for ${SSH_HOSTNAME} (client-key authentication still applies)"
  elif ! check_initrd_checksum
  then
    return 1
  else
    log "Initrd checksum and signature of ${SSH_HOSTNAME} are valid"
  fi

  log "Sending LUKS passphrase to ${SSH_HOSTNAME} (method: ${LUKS_TYPE})"

  if ! luks_unlock
  then
    local reason=''
    if [[ -n "$UNLOCK_OUTPUT_TAIL" ]]
    then
      reason=" Last output from ${SSH_HOSTNAME}: ${UNLOCK_OUTPUT_TAIL}."
    fi
    log_state --notify unlock-failed failure \
      "Failed to unlock ${SSH_HOSTNAME}: sending the LUKS passphrase did not succeed (method: ${LUKS_TYPE}).${reason} Retrying every ${SLEEP_INTERVAL}s"
    return 1
  fi

  log_state --notify unlocked success "Unlocked ${SSH_HOSTNAME}; it should finish booting shortly"
  return 0
}

run_action() {
  if [[ -z "$LUKS_PASSPHRASE" ]] && [[ -n "$LUKS_PASSPHRASE_FILE" ]]
  then
    if [[ ! -r "$LUKS_PASSPHRASE_FILE" ]]
    then
      echo "$LUKS_PASSPHRASE_FILE: No such file or directory" >&2
      return 3
    fi

    LUKS_PASSPHRASE="$(cat "$LUKS_PASSPHRASE_FILE")"
  fi

  if [[ -z "$LUKS_PASSPHRASE" ]]
  then
    echo "LUKS_PASSPHRASE is not set." >&2
    return 2
  fi

  if [[ -n "$FORCE" && -z "$RUN_ONCE" ]]
  then
    echo "--force can only be used with --once." >&2
    return 2
  fi

  log "Watching ${SSH_HOSTNAME} ($(target_route), LUKS method: ${LUKS_TYPE}), checking every ${SLEEP_INTERVAL}s"

  if [[ -n "$RUN_ONCE" ]]
  then
    run_tick
    return "$?"
  fi

  while true
  do
    run_tick &
    local pid=$!
    local count=0

    while kill -0 "$pid" 2>/dev/null
    do
      if (( count >= TICK_TIMEOUT ))
      then
        log -w "Check of ${SSH_HOSTNAME} took longer than ${TICK_TIMEOUT}s; aborting it"
        kill "$pid"

        # Wait a bit for it to die
        local kill_wait=0
        while kill -0 "$pid" 2>/dev/null
        do
          sleep 1
          ((kill_wait++))
          if (( kill_wait > 10 ))
          then
            log -w "Check process $pid did not exit; sending SIGKILL"
            kill -9 "$pid"
            break
          fi
        done

        break
      fi
      sleep 1
      ((count++))
    done

    wait "$pid" 2>/dev/null

    sleep "$SLEEP_INTERVAL"
  done
}

main() {
  while [[ -n "$*" ]]
  do
    case "$1" in
      run|status)
        ACTION="$1"
        shift
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      --debug|-D)
        DEBUG=1
        shift
        ;;
      --once)
        RUN_ONCE=1
        shift
        ;;
      --force|-f)
        FORCE=1
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
      --ssh-known-hosts)
        SSH_KNOWN_HOSTS="$2"
        shift 2
        ;;
      --ssh-known-hosts-file)
        SSH_KNOWN_HOSTS_FILE="$2"
        shift 2
        ;;
      --ssh-initrd-known-hosts)
        SSH_INITRD_KNOWN_HOSTS="$2"
        shift 2
        ;;
      --ssh-initrd-known-hosts-file)
        SSH_INITRD_KNOWN_HOSTS_FILE="$2"
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
      --tick-timeout|--timeout)
        TICK_TIMEOUT="$2"
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
      --luks-passphrase|--luks-password|--password|-p)
        LUKS_PASSPHRASE="$2"
        log -w "Passing the LUKS passphrase on the command line exposes it to other users (ps, /proc/*/cmdline) and shell history; use --luks-passphrase-file or LUKS_PASSPHRASE_FILE instead"
        shift 2
        ;;
      --luks-passphrase-file|--luks-password-file|-F)
        LUKS_PASSPHRASE_FILE="$2"
        shift 2
        ;;
      --initrd-checksum-file)
        INITRD_CHECKSUM_FILE="$2"
        shift 2
        ;;
      --initrd-checksum-dir)
        INITRD_CHECKSUM_DIR="$2"
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
      --healthcheck-known-hosts-type)
        SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE="$2"
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
      --msmtp-email-account|--email-account)
        MSMTP_ACCOUNT="$2"
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
      echo "DEBUG: Secrets (/run/secrets)"
      ls -l /run/secrets
    } >&2
  fi

  if [[ ! -r "$SSH_KEY" ]]
  then
    echo "SSH_KEY at $SSH_KEY is not readable." >&2
    return 2
  fi

  # Copy file locally and correct mode
  if [[ "$(stat -c "%a" "$SSH_KEY")" != "400" ]]
  then
    cp "$SSH_KEY" "$TMPDIR"
    SSH_KEY="${TMPDIR}/$(basename "$SSH_KEY")"
    chmod 400 "$SSH_KEY"
  fi

  case "$ACTION" in
    run)
      run_action
      ;;
    status)
      show_status
      ;;
    *)
      printf 'Unknown action: %s\n' "$ACTION" >&2
      usage >&2
      return 2
      ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi

# vim: set ft=sh et ts=2 sw=2 :
