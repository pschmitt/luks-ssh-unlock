#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

# The measuring is done by initrd-measure, a static binary per target arch
# (initrd-measure-$(uname -m) in INITRD_CHECKSUM_MEASURE_DIR, by default the
# directory of this script). For a remote host it is uploaded over SSH and
# run there, so nothing of the target's userland does the measuring: only its
# shell, mkdir, cat and chmod put the binary in place.
#
# Requirements (binaries): bash, coreutils (mktemp, sort, cat, uname,
# dirname), diffutils (diff), openssh-client (ssh, remote modes).

usage() {
  cat <<EOF
Usage: $(basename "$0") [ACTION] [OPTIONS]
  or:  $(basename "$0") diff FILE1 FILE2
Hash initrd contents locally or over SSH, or diff two checksum files.

Actions:
  checksum                Detect and hash initrd or live root (default action).
  diff FILE1 FILE2        Unified diff of two existing checksum files (sorted).

Mandatory arguments to long options are mandatory for short options too.

Options (checksum):
  -H, --host HOST         SSH to HOST; auto-detect initrd via /etc/initrd-release.
  -l, --ssh-user USER     SSH user (default: root).
  -i, --identity FILE     SSH identity file to use.
      --initrd[=PATH]     Force hashing of an initrd image (default: /run/current-system/initrd).
      --diff FILE         After hashing, show diff vs FILE (sorted).
      --format 1|2        Output format (default: that of the --diff FILE, else 2).
                          1 lists regular files only; 2 adds symlinks and mount points.
  -q, --quiet             Suppress checksum stdout and routine info logs; warnings/errors remain.
      --debug             Enable verbose debug and info logs (overrides --quiet for logs).
      --paranoid          Accepted for compatibility; the measurer is always uploaded.
      --insecure-ssh      Disable host key verification (unsafe; sets StrictHostKeyChecking=no and null known_hosts files).
      --known-hosts-file FILE
                          Override the SSH UserKnownHostsFile (only in secure mode).
  -o, --ssh-option OPT    Pass "-o OPT" to every ssh invocation (repeatable),
                          e.g. a ProxyCommand to reach the host via a jumphost.
  -h, --help              Show this help and exit.

Output:
  SHA256_HASH  /absolute/path  (for checksum)
  unified diff                 (for diff)

Notes:
  - Live root measurement expects root privileges.
  - SSH defaults to user "root". Override with --ssh-user/--ssh-username/-l.
EOF
}

ssh() {
  log_debug "\$ ssh ${SSH_EXTRA_OPTS[*]} $*"
  command ssh "${SSH_EXTRA_OPTS[@]}" "$@"
}

setup_colors() {
  if [[ -t 1 ]]
  then
    COLOR_DEBUG=$'\033[1;35m'
    COLOR_INFO=$'\033[1;34m'
    COLOR_WARN=$'\033[1;33m'
    COLOR_ERR=$'\033[1;31m'
    COLOR_OK=$'\033[1;32m'
    COLOR_RESET=$'\033[0m'
  else
    COLOR_DEBUG=""
    COLOR_INFO=""
    COLOR_WARN=""
    COLOR_ERR=""
    COLOR_OK=""
    COLOR_RESET=""
  fi
}

log() {
  local level=$1 msg=$2 color=$3
  printf '%s%s%s %s\n' "$color" "$level" "$COLOR_RESET" "$msg" >&2
}

log_debug() {
  if [[ -n "$DEBUG" ]]
  then
    log DBG "$1" "$COLOR_DEBUG"
  fi
}

log_info() {
  if [[ -z "$LOG_QUIET" || -n "$DEBUG" ]]
  then
    log INF "$1" "$COLOR_INFO"
  fi
}

log_warn() {
  log WRN "$1" "$COLOR_WARN"
}

log_err() {
  log ERR "$1" "$COLOR_ERR"
}

log_ok() {
  log OK "$1" "$COLOR_OK"
}

EXIT_CMDS=()

run_exit_traps() {
  local cmd
  for cmd in "${EXIT_CMDS[@]}"
  do
    eval "$cmd"
  done
}

add_exit_trap() {
  local new_cmd="$1"
  EXIT_CMDS+=("$new_cmd")
  trap run_exit_traps EXIT
}

SSH_OPTS_SECURE=(
  -o ControlMaster=no
)

SSH_OPTS_INSECURE=(
  -o ControlMaster=no
  -o UserKnownHostsFile=/dev/null
  -o GlobalKnownHostsFile=/dev/null
  -o StrictHostKeyChecking=no
)

SSH_OPTS=()
# Extra -o options from --ssh-option; kept separate from SSH_OPTS so that
# toggling secure/insecure mode does not drop them.
SSH_EXTRA_OPTS=()
SSH_KNOWN_HOSTS_FILE=""
SSH_IDENTITY_FILE=""
INSECURE_SSH=0
SCRIPT_PATH=$(readlink -f "${BASH_SOURCE[0]:-$0}" 2>/dev/null || printf '%s' "$0")

enable_secure_ssh() {
  INSECURE_SSH=0
  SSH_OPTS=("${SSH_OPTS_SECURE[@]}")
  apply_known_hosts_file
  apply_identity_file
}

enable_insecure_ssh() {
  log_warn "insecure SSH mode: host key validation disabled; susceptible to MITM"
  if [[ -n "$SSH_KNOWN_HOSTS_FILE" ]]
  then
    log_warn "--known-hosts-file ignored while --insecure-ssh is active"
  fi
  INSECURE_SSH=1
  SSH_OPTS=("${SSH_OPTS_INSECURE[@]}")
  apply_identity_file
}

apply_known_hosts_file() {
  if [[ -z "$SSH_KNOWN_HOSTS_FILE" ]]
  then
    return
  fi

  if (( INSECURE_SSH == 1 ))
  then
    log_warn "--known-hosts-file ignored while --insecure-ssh is active"
    return
  fi

  local user_known_hosts_file=$SSH_KNOWN_HOSTS_FILE
  SSH_OPTS+=( -o "UserKnownHostsFile=${user_known_hosts_file}" )
}

apply_identity_file() {
  if [[ -z "$SSH_IDENTITY_FILE" ]]
  then
    return
  fi

  SSH_OPTS+=( -i "$SSH_IDENTITY_FILE" )
}

enable_secure_ssh

MEASURE_DIR="${INITRD_CHECKSUM_MEASURE_DIR:-$(dirname "$SCRIPT_PATH")}"
FORMAT_V2_HEADER="# initrd-measure format 2"

# The measurer binary for ARCH (as `uname -m` prints it). A remote ARCH comes
# from the target, so it must never pick any other file than a measurer.
measure_bin() {
  local arch=$1 bin=""

  case "$arch" in
    aarch64|arm64)
      arch=aarch64
    ;;
    x86_64|amd64)
      arch=x86_64
    ;;
    *)
      log_err "unsupported architecture: $(printf '%q' "$arch")"
      return 1
    ;;
  esac

  bin="${MEASURE_DIR}/initrd-measure-${arch}"
  if [[ -x "$bin" ]]
  then
    printf '%s\n' "$bin"
    return 0
  fi

  # A plain build (cargo install) for this machine only.
  if [[ "$arch" == "$(uname -m)" ]] && bin=$(command -v initrd-measure)
  then
    printf '%s\n' "$bin"
    return 0
  fi

  log_err "no initrd-measure binary for $arch (looked for ${MEASURE_DIR}/initrd-measure-${arch})"
  return 1
}

# The format of an existing checksum file: 2 if it starts with the format 2
# header, 1 otherwise (the original shell output).
file_format() {
  local file=$1 first=""

  if [[ -r "$file" ]] && IFS= read -r first < "$file" && [[ "$first" == "$FORMAT_V2_HEADER" ]]
  then
    printf '2\n'
    return 0
  fi
  printf '1\n'
}

# Single-quote a word for a POSIX shell.
sh_quote() {
  printf "'%s'" "${1//\'/\'\\\'\'}"
}

# The measurer's arguments, in MEASURE_ARGS: auto-detect (the live root
# inside an initrd, else the image), or always the image.
measure_args() {
  local format=$1 initrd_mode=$2 initrd_path=$3

  MEASURE_ARGS=(--format "$format")
  if [[ -n "$initrd_mode" ]]
  then
    MEASURE_ARGS+=(image)
  else
    MEASURE_ARGS+=(auto)
  fi
  if [[ -n "$initrd_path" ]]
  then
    MEASURE_ARGS+=("$initrd_path")
  fi
}

measure_local() {
  local format=$1 initrd_mode=$2 initrd_path=$3 bin=""

  bin=$(measure_bin "$(uname -m)") || return 1
  measure_args "$format" "$initrd_mode" "$initrd_path"
  log_info "local mode=${MEASURE_ARGS[2]} format=$format measurer=$bin"
  "$bin" "${MEASURE_ARGS[@]}"
}

measure_remote() {
  local host=$1 ssh_user=$2 format=$3 initrd_mode=$4 initrd_path=$5
  local arch="" bin="" args="" arg="" remote_cmd=""

  if ! arch=$(ssh "${SSH_OPTS[@]}" -l "$ssh_user" "$host" -- uname -m)
  then
    log_err "cannot detect the architecture of $host"
    return 1
  fi
  bin=$(measure_bin "$arch") || return 1
  log_info "remote arch detected: $arch"

  measure_args "$format" "$initrd_mode" "$initrd_path"
  for arg in "${MEASURE_ARGS[@]}"
  do
    args+=" $(sh_quote "$arg")"
  done

  # One connection: stdin is the binary, written to a private directory
  # (in /run, else a mktemp one for an unprivileged user), run and removed.
  # Plain POSIX sh: the target's shell may be busybox ash.
  remote_cmd="umask 077"
  remote_cmd+=" && d=/run/initrd-measure.\$\$"
  remote_cmd+=" && { mkdir \"\$d\" 2>/dev/null || d=\$(mktemp -d) || exit 97; }"
  remote_cmd+=" && trap 'rm -rf \"\$d\"' EXIT"
  remote_cmd+=" && cat > \"\$d/initrd-measure\""
  remote_cmd+=" && chmod 700 \"\$d/initrd-measure\""
  remote_cmd+=" && \"\$d/initrd-measure\"${args}"

  log_info "remote host=$host user=$ssh_user mode=${MEASURE_ARGS[2]} format=$format measurer=$bin"
  ssh "${SSH_OPTS[@]}" -l "$ssh_user" "$host" -- "$remote_cmd" < "$bin"
}

diff_hashes() {
  local file1=$1
  local file2=$2

  if [[ ! -f "$file1" ]]
  then
    log_err "file not found: $file1"
    exit 1
  fi

  if [[ ! -f "$file2" ]]
  then
    log_err "file not found: $file2"
    exit 1
  fi

  log_info "diffing files"
  log_debug "diff -u <(sort \"$file1\") <(sort \"$file2\")"
  if diff -u <(sort "$file1") <(sort "$file2")
  then
    log_ok "files are identical"
    return 0
  fi

  return 1
}

run_checksum() {
  local host=$1 initrd_path=$2 initrd_mode=$3 ssh_user=$4
  local diff_target=$5 quiet=$6 format=$7
  local tmpfile=""

  if [[ -z "$format" ]]
  then
    format=2
    if [[ -n "$diff_target" ]]
    then
      format=$(file_format "$diff_target")
    fi
  fi

  tmpfile=$(mktemp)
  add_exit_trap "rm -f '$tmpfile'"

  if [[ -n "$host" ]]
  then
    measure_remote "$host" "$ssh_user" "$format" "$initrd_mode" "$initrd_path" > "$tmpfile" || return 1
  else
    measure_local "$format" "$initrd_mode" "$initrd_path" > "$tmpfile" || return 1
  fi

  if [[ -n "$quiet" ]]
  then
    log_info "quiet mode enabled; checksum output suppressed (temporary file: $tmpfile; will be removed on exit)"
  else
    cat "$tmpfile"
  fi

  if [[ -n "$diff_target" ]]
  then
    log_info "diffing checksum output against $diff_target"
    diff_hashes "$diff_target" "$tmpfile"
  fi
}

main() {
  setup_colors

  local action="" host="" initrd_mode="" initrd_path=""
  local diff_file1="" diff_file2="" checksum_diff="" quiet=""
  local known_hosts_file="" identity_file="" ssh_user="root" format=""
  DEBUG="${DEBUG:-}"
  LOG_QUIET="${LOG_QUIET:-}"

  if [[ $# -gt 0 ]]
  then
    case "$1" in
      --help|-h)
        usage
        exit 0
      ;;
    esac
  fi

  action=${1:-checksum}
  if [[ $# -gt 0 ]]
  then
    shift
  fi

  while [[ $# -gt 0 ]]
  do
    case "$1" in
      --host|--hostname|-H)
        if [[ $# -lt 2 ]]
        then
          log_err "missing argument for $1"
          exit 2
        fi
        host=$2
        shift 2
      ;;
      --initrd)
        initrd_mode=1
        if [[ $# -gt 1 ]]
        then
          case "$2" in
            -*)
              :
            ;;
            *)
              initrd_path=$2
              shift
            ;;
          esac
        fi
        shift
      ;;
      --initrd=*)
        initrd_mode=1
        initrd_path=${1#--initrd=}
        shift
      ;;
      --ssh-user|--ssh-username|-l)
        if [[ $# -lt 2 ]]
        then
          log_err "missing argument for $1"
          exit 2
        fi
        ssh_user=$2
        shift 2
      ;;
      --identity|-i)
        if [[ $# -lt 2 ]]
        then
          log_err "missing argument for $1"
          exit 2
        fi
        identity_file=$2
        SSH_IDENTITY_FILE=$identity_file
        apply_identity_file
        shift 2
      ;;
      --diff)
        if [[ "$action" == "diff" ]]
        then
          if [[ -z "$diff_file1" ]]
          then
            diff_file1=$2
          elif [[ -z "$diff_file2" ]]
          then
            diff_file2=$2
          else
            log_err "too many arguments for diff"
            exit 2
          fi
          shift 2
        else
          if [[ $# -lt 2 ]]
          then
            log_err "missing argument for --diff FILE"
            exit 2
          fi
          checksum_diff=$2
          shift 2
        fi
      ;;
      --help|-h)
        usage
        exit 0
      ;;
      -q|--quiet)
        quiet=1
        LOG_QUIET=1
        shift
      ;;
      --debug|-D)
        DEBUG=1
        shift
      ;;
      --paranoid)
        log_debug "--paranoid is the default now: the measurer is always uploaded"
        shift
      ;;
      --format)
        if [[ $# -lt 2 ]]
        then
          log_err "missing argument for $1"
          exit 2
        fi
        format=$2
        shift 2
      ;;
      --format=*)
        format=${1#--format=}
        shift
      ;;
      --insecure*)
        enable_insecure_ssh
        shift
      ;;
      --known-hosts-file)
        if [[ $# -lt 2 ]]
        then
          log_err "missing argument for $1"
          exit 2
        fi
        known_hosts_file=$2
        SSH_KNOWN_HOSTS_FILE=$known_hosts_file
        apply_known_hosts_file
        shift 2
      ;;
      --ssh-option|-o)
        if [[ $# -lt 2 ]]
        then
          log_err "missing argument for $1"
          exit 2
        fi
        SSH_EXTRA_OPTS+=(-o "$2")
        shift 2
      ;;
      --ssh-option=*)
        SSH_EXTRA_OPTS+=(-o "${1#--ssh-option=}")
        shift
      ;;
      --known-hosts-file=*)
        known_hosts_file=${1#--known-hosts-file=}
        SSH_KNOWN_HOSTS_FILE=$known_hosts_file
        apply_known_hosts_file
        shift
      ;;
      *)
        if [[ "$action" == "diff" ]]
        then
          if [[ -z "$diff_file1" ]]
          then
            diff_file1=$1
          elif [[ -z "$diff_file2" ]]
          then
            diff_file2=$1
          else
            log_err "unexpected argument: $1"
            exit 2
          fi
          shift
        else
          log_err "unknown argument: $1"
          usage
          exit 2
        fi
      ;;
    esac
  done

  case "$format" in
    ""|1|2)
      :
    ;;
    *)
      log_err "--format takes 1 or 2"
      exit 2
    ;;
  esac

  case "$action" in
    checksum|check)
      if [[ -n "$host" && "$host" == *@* ]]
      then
        log_err "use --ssh-user/--ssh-username/-l instead of embedding '@' in --host"
        exit 2
      fi

      run_checksum "$host" "$initrd_path" "$initrd_mode" "$ssh_user" "$checksum_diff" "$quiet" "$format"
    ;;
    diff)
      if [[ -z "$diff_file1" || -z "$diff_file2" ]]
      then
        log_err "diff mode needs two files"
        exit 2
      fi

      diff_hashes "$diff_file1" "$diff_file2"
    ;;
    *)
      log_err "unknown action: $action"
      usage
      exit 2
    ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi

# vim: set ft=sh et ts=2 sw=2 :
