#!/usr/bin/env bash

SSH_HOSTNAME="${SSH_HOSTNAME:-example.com}"
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
LUKS_TYPE="${LUKS_TYPE:-raw}"

DEBUG="${DEBUG:-}"
PARANOID="${PARANOID:-}"
EVENTS_FILE="${EVENTS_FILE:-}"
SKIP_SSH_PORT_CHECK="${SKIP_SSH_PORT_CHECK:-}"
SLEEP_INTERVAL="${SLEEP_INTERVAL:-10}"
TMPDIR="${TMPDIR:-/tmp}"

HEALTHCHECK_PORT="${HEALTHCHECK_PORT:-}"
HEALTHCHECK_REMOTE_CMD="${HEALTHCHECK_REMOTE_CMD:-}"
HEALTHCHECK_REMOTE_HOSTNAME="${HEALTHCHECK_REMOTE_HOSTNAME:-}"
HEALTHCHECK_REMOTE_USERNAME="${HEALTHCHECK_REMOTE_USERNAME:-}"
SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE="${SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE:-default}"

INITRD_CHECKSUM_FILE="${INITRD_CHECKSUM_FILE:-}"
INITRD_CHECKSUM_DIR="${INITRD_CHECKSUM_DIR:-}"
INITRD_CHECKSUM_SCRIPT="${INITRD_CHECKSUM_SCRIPT:-/app/bin/initrd-checksum}"

APPRISE_TAG="${APPRISE_TAG:-}"
APPRISE_TITLE="${APPRISE_TITLE:-}"
APPRISE_URL="${APPRISE_URL:-}"

EMAIL_FROM="${EMAIL_FROM:-}"
EMAIL_RECIPIENT="${EMAIL_RECIPIENT:-}"
EMAIL_SUBJECT="${EMAIL_SUBJECT:-}"
MSMTP_ACCOUNT="${MSMTP_ACCOUNT:-}"

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
  echo "  --skip-ssh-port-check, --skip-port-check"
  echo "                 Skip SSH port check"
  echo "                 Env var: SKIP_SSH_PORT_CHECK"
  echo

  echo "  --luks-type, --type, -t TYPE"
  echo "                 LUKS type to use (raw, systemd, systemd-tool, luks-mount)"
  echo "                 Env var: LUKS_TYPE"
  echo "  --luks-passphrase, --luks-password, --password, -p PASSWORD"
  echo "                 LUKS password to use"
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

log() {
  echo "$(date -Iseconds) $*" >&2
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
    msg=${msg//#luks_password/${LUKS_PASSPHRASE}}
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

  if [[ -z "$known_hosts_file" ]]
  then
    known_hosts_file=/dev/null
  fi

  ssh_opts+=(-o "UserKnownHostsFile=${known_hosts_file}")

  if [[ "$known_hosts_file" == /dev/null ]]
  then
    ssh_opts+=(-o StrictHostKeyChecking=no)
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

  if [[ "$known_hosts_file" == /dev/null ]]
  then
    scp_opts+=(-o StrictHostKeyChecking=no)
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
    local proxy_opts=(-F /dev/null -o ConnectTimeout="$SSH_CONNECTION_TIMEOUT")
    proxy_opts+=(-o "UserKnownHostsFile=${known_hosts_file}")
    if [[ "$known_hosts_file" == /dev/null ]]
    then
      proxy_opts+=(-o StrictHostKeyChecking=no)
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
    proxy_cmd=${proxy_cmd% }
    extra_args=(-o "ProxyCommand=ssh ${proxy_cmd}")
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
  log "Resolved $SSH_HOSTNAME to $resolved_hostname"

  if [[ -n "$SSH_JUMPHOST" ]]
  then
    echo | _ssh_jumphost nc "$resolved_hostname" "$SSH_PORT" 2>&1 | \
      grep -iE "^SSH-"
    return "$?"
  fi

  nc -z -w 2 "$resolved_hostname" "$SSH_PORT"
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

  local known_hosts_file
  known_hosts_file=$(_known_hosts_path initrd) || return 1

  local checksum_args=(checksum --host "$SSH_HOSTNAME" --ssh-user "$SSH_USERNAME" --diff "$INITRD_CHECKSUM_FILE")
  if [[ "$known_hosts_file" == /dev/null ]]
  then
    checksum_args+=(--insecure-ssh)
  elif [[ -n "$known_hosts_file" ]]
  then
    checksum_args+=(--known-hosts-file "$known_hosts_file")
  fi

  if [[ -n "$PARANOID" ]]
  then
    checksum_args+=(--paranoid)
  fi

  if [[ -n "$DEBUG" ]]
  then
    log "Running initrd checksum validation via ${checksum_script}"
  fi

  if "$checksum_script" "${checksum_args[@]}"
  then
    return 0
  fi

  log-notify -w "Initrd checksum validation failed for ${SSH_HOSTNAME}; skipping unlock attempt"
  return 1
}

fetch_initrd_checksum() {
  if [[ -z "$INITRD_CHECKSUM_DIR" ]]
  then
    return 0
  fi

  local remote_checksum_path="/etc/initrd-checksum"
  local checksum_dir="${INITRD_CHECKSUM_DIR%/}/${SSH_HOSTNAME}"

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

  if [[ -n "$DEBUG" ]]
  then
    log "Stored initrd checksum from ${SSH_HOSTNAME} to ${checksum_dir}"
  fi

  return 0
}

luks_unlock() {
  local SSH_KNOWN_HOSTS_TYPE_OVERRIDE=initrd

  case "$LUKS_TYPE" in
    raw|direct)
      _ssh <<< "$LUKS_PASSPHRASE"
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

      if ! _ssh cryptsetup luksOpen "$disk" "$mapper" - <<< "$LUKS_PASSPHRASE"
      then
        echo "Failed to unlock disk $disk" >&2
        return 1
      fi

      _ssh systemctl restart "systemd-cryptsetup@${mapper}"
      ;;

    # https://github.com/gsauthof/dracut-sshd/issues/32
    systemd|dracut-systemd|dracut-sshd|dracut|alt)
      _ssh -tt systemd-tty-ask-password-agent <<< "$LUKS_PASSPHRASE"
      ;;

    # https://github.com/pschmitt/luks-mount.sh
    luks-mount)
      _ssh luks-mount <<< "$LUKS_PASSPHRASE"
      ;;
  esac
}

main() {
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
    exit 2
  fi

  # Copy file locally and correct mode
  if [[ "$(stat -c "%a" "$SSH_KEY")" != "400" ]]
  then
    cp "$SSH_KEY" "$TMPDIR"
    SSH_KEY="${TMPDIR}/$(basename "$SSH_KEY")"
    chmod 400 "$SSH_KEY"
  fi

  if [[ -z "$LUKS_PASSPHRASE" ]] && [[ -n "$LUKS_PASSPHRASE_FILE" ]]
  then
    if [[ ! -r "$LUKS_PASSPHRASE_FILE" ]]
    then
      echo "$LUKS_PASSPHRASE_FILE: No such file or directory" >&2
      exit 3
    fi

    LUKS_PASSPHRASE="$(cat "$LUKS_PASSPHRASE_FILE")"
  fi

  if [[ -z "$LUKS_PASSPHRASE" ]]
  then
    echo "LUKS_PASSPHRASE is not set." >&2
    exit 2
  fi

  local msg="LUKS rigmarole started (type: $LUKS_TYPE). I'll be trying to unlock ${SSH_HOSTNAME}"
  if [[ -n "$SSH_JUMPHOST" ]]
  then
    msg+=" through ${SSH_JUMPHOST}"
  fi
  log "$msg"

  while true
  do
    # Perform Healthcheck if required
    if [[ -n "$HEALTHCHECK_PORT" ]]
    then
      if nc -z -w 2 "$SSH_HOSTNAME" "$HEALTHCHECK_PORT"
      then
        fetch_initrd_checksum
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
         SSH_KNOWN_HOSTS_TYPE_OVERRIDE=${SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE:-default} \
        _ssh sh -c "$HEALTHCHECK_REMOTE_CMD"
      then
        fetch_initrd_checksum
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
      if ! check_initrd_checksum
      then
        sleep "$SLEEP_INTERVAL"
        continue
      fi

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
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi
