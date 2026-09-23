{
  config,
  lib,
  pkgs,
  ...
}:

with lib;

let
  cfg = config.services.luks-ssh-unlock;
  pathOrStr = types.either types.path types.str;
  nullPathOrStr = types.nullOr pathOrStr;
  package = pkgs.callPackage ./package.nix {
    busyboxBundle = pkgs.callPackage ./busybox.nix {
      busyboxAmd64 = pkgs.pkgsStatic.busybox;
      busyboxArm64 = pkgs.pkgsStatic.busybox;
    };
  };
  adHocUnlockers = mapAttrsToList (
    name: _:
    pkgs.writeShellApplication {
      name = "luks-ssh-unlock-${name}";
      runtimeInputs = [ pkgs.systemd ];
      text = ''
        case "''${1:-}" in
          logs|l)
            shift
            exec journalctl \
              --follow \
              --unit=luks-ssh-unlock-${name}.service \
              "$@" \
              --output=cat
            ;;
          status)
            if [[ $# -ne 1 ]]
            then
              printf 'Usage: luks-ssh-unlock-${name} status\n' >&2
              exit 2
            fi
            status_exit=0
            status_environment=()
            if [[ -t 1 && "''${TERM:-}" != dumb ]]
            then
              status_environment+=(--property=Environment=FORCE_COLOR=1)
            fi
            systemd-run \
              --system \
              --wait \
              --pipe \
              --collect \
              --unit=${escapeShellArg "luks-ssh-unlock-status-${name}-"}"$$" \
              --property=${escapeShellArg "EnvironmentFile=/etc/luks-ssh-unlock/${name}.env"} \
              "''${status_environment[@]}" \
              -- ${package}/bin/luks-ssh-unlock status || status_exit=$?
            last_unlock=$(journalctl \
              --grep=${escapeShellArg "LUKS unlocked host at ${name}"} \
              --no-pager \
              --output=short-iso \
              -n 1) || last_unlock=
            if [[ -n "$last_unlock" ]]
            then
              printf '\nLast reported unlock for ${name}: %s\n' "$last_unlock"
            else
              printf '\nLast reported unlock for ${name}: no successful unlock recorded\n'
            fi
            printf '\nRecent daemon logs for ${name}:\n'
            log_lines=$(journalctl --unit=luks-ssh-unlock-${name}.service --no-pager --output=cat -n 5) || log_lines=
            log_color=
            log_reset=
            if [[ -n "''${FORCE_COLOR:-}" || ( -t 1 && "''${TERM:-}" != dumb ) ]]
            then
              log_color=$'\033[90m'
              log_reset=$'\033[0m'
            fi
            if [[ -n "$log_lines" ]]
            then
              while IFS= read -r log_line
              do
                printf '%s%s%s\n' "$log_color" "$log_line" "$log_reset"
              done <<< "$log_lines"
            fi
            exit "$status_exit"
            ;;
        esac

        exec systemd-run \
          --system \
          --wait \
          --pipe \
          --collect \
          --unit=${escapeShellArg "luks-ssh-unlock-manual-${name}-"}"$$" \
          --property=${escapeShellArg "EnvironmentFile=/etc/luks-ssh-unlock/${name}.env"} \
          --property=TimeoutStartSec=infinity \
          -- ${package}/bin/luks-ssh-unlock run --once "$@"
      '';
    }
  ) cfg.instances;
in
{
  config = mkIf cfg.enable {
    environment.systemPackages = [ package ] ++ adHocUnlockers;

    # Define environment files
    environment.etc =
      (mapAttrs' (
        name: instance:
        nameValuePair "luks-ssh-unlock/${name}.env" {
          text =
            with instance;
            let
              knownHostsPath =
                if sshKnownHosts != "" then
                  "/etc/luks-ssh-unlock/${name}.known_hosts"
                else if sshKnownHostsFile != null then
                  toString sshKnownHostsFile
                else
                  "";
              initrdKnownHostsPath =
                if initrdKnownHosts != "" then
                  "/etc/luks-ssh-unlock/${name}.initrd_known_hosts"
                else if initrdKnownHostsFile != null then
                  toString initrdKnownHostsFile
                else
                  "";
              initrdChecksumDir =
                if initrdCheck.dir != null then toString initrdCheck.dir else null;
              initrdChecksumFile =
                if initrdCheck.file != null then
                  toString initrdCheck.file
                else if initrdChecksumDir != null then
                  "${initrdChecksumDir}/${hostname}/initrd-checksum/checksum"
                else
                  null;
            in
            ''
              DEBUG=${optionalString debug "1"}
              SLEEP_INTERVAL=${toString sleepInterval}
              TICK_TIMEOUT=${toString tickTimeout}

              SSH_HOSTNAME=${hostname}
              SSH_USERNAME=${username}
              SSH_CONNECTION_TIMEOUT=${toString connectionTimeout}
              SSH_KEY=${toString key}
              SSH_PORT=${toString port}

              ${optionalString forceIpv4 ''
                FORCE_IPV4=1
              ''}
              ${optionalString forceIpv6 ''
                FORCE_IPV6=1
              ''}

              ${optionalString (knownHostsPath != "") ''
                SSH_KNOWN_HOSTS_FILE=${knownHostsPath}
              ''}
              ${optionalString (initrdKnownHostsPath != "") ''
                SSH_INITRD_KNOWN_HOSTS_FILE=${initrdKnownHostsPath}
              ''}

              ${optionalString (jumpHost != null && jumpHost.enable) ''
                SSH_JUMPHOST=${jumpHost.hostname}
                SSH_JUMPHOST_USERNAME=${jumpHost.username}
                SSH_JUMPHOST_PORT=${toString jumpHost.port}
                SSH_JUMPHOST_KEY=${toString jumpHost.key}
              ''}

              LUKS_TYPE=${type}
              ${optionalString (passphrase != "") ''
                LUKS_PASSPHRASE=${passphrase}
              ''}
              ${optionalString (passphraseFile != null) ''
                LUKS_PASSPHRASE_FILE=${toString passphraseFile}
              ''}

              ${optionalString initrdCheck.enable ''
                ${optionalString (initrdChecksumFile != null) ''
                  INITRD_CHECKSUM_FILE=${initrdChecksumFile}
                ''}
                ${optionalString (initrdChecksumDir != null) ''
                  INITRD_CHECKSUM_DIR=${initrdChecksumDir}
                ''}
                INITRD_CHECKSUM_SCRIPT=${toString initrdCheck.script}
                INITRD_CHECKSUM_BUSYBOX_DIR=${toString initrdCheck.busyboxDir}
                ${optionalString initrdCheck.paranoid ''
                  PARANOID=1
                ''}
                ${optionalString initrdCheck.requireSignature ''
                  INITRD_CHECKSUM_REQUIRE_SIGNATURE=1
                ''}
              ''}

              ${optionalString (eventsFile != null) ''
                EVENTS_FILE=${toString eventsFile}
              ''}
              ${optionalString skipSshPortCheck ''
                SKIP_SSH_PORT_CHECK=1
              ''}
              ${optionalString (tmpdir != null) ''
                TMPDIR=${toString tmpdir}
              ''}

              ${optionalString notifications.enable ''
                ${optionalString notifications.apprise.enable ''
                  ${optionalString (notifications.apprise.url != "") ''
                    APPRISE_URL=${escapeShellArg notifications.apprise.url}
                  ''}
                  ${optionalString (notifications.apprise.tag != "") ''
                    APPRISE_TAG=${escapeShellArg notifications.apprise.tag}
                  ''}
                  ${optionalString (notifications.apprise.title != "") ''
                    APPRISE_TITLE=${escapeShellArg notifications.apprise.title}
                  ''}
                ''}

                ${optionalString (notifications.msmtp.account != "") ''
                  MSMTP_ACCOUNT=${escapeShellArg notifications.msmtp.account}
                ''}

                ${optionalString notifications.mail.enable ''
                  EMAIL_RECIPIENT="${optionalString (notifications.mail.recipient != "") notifications.mail.recipient}"
                  EMAIL_FROM="${optionalString (notifications.mail.from != "") notifications.mail.from}"
                  EMAIL_SUBJECT=${escapeShellArg notifications.mail.subject}
                ''}
              ''}

              ${optionalString healthcheck.enable ''
                HEALTHCHECK_PORT=${optionalString (healthcheck.port != null) (toString healthcheck.port)}
                HEALTHCHECK_REMOTE_HOSTNAME="${optionalString (healthcheck.hostname != "") healthcheck.hostname}"
                HEALTHCHECK_REMOTE_USERNAME="${optionalString (healthcheck.username != "") healthcheck.username}"
                HEALTHCHECK_REMOTE_CMD="${healthcheck.command}"
                SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE=${healthcheck.knownHostsType}
              ''}

              ${concatStringsSep "\n" (
                mapAttrsToList (k: v: "${k}=${escapeShellArg v}") extraEnvironment
              )}
            '';
        }
      ) cfg.instances)
      // (mapAttrs' (
        name: instance:
        nameValuePair "luks-ssh-unlock/${name}.known_hosts" { text = instance.sshKnownHosts; }
      ) (filterAttrs (_: instance: instance.sshKnownHosts != "") cfg.instances))
      // (mapAttrs' (
        name: instance:
        nameValuePair "luks-ssh-unlock/${name}.initrd_known_hosts" { text = instance.initrdKnownHosts; }
      ) (filterAttrs (_: instance: instance.initrdKnownHosts != "") cfg.instances));

    assertions =
      (mapAttrsToList (name: instance: {
        assertion = !(instance.sshKnownHosts != "" && instance.sshKnownHostsFile != null);
        message = ''services.luks-ssh-unlocker.instances.${name} cannot set both sshKnownHosts and sshKnownHostsFile'';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(instance.initrdKnownHosts != "" && instance.initrdKnownHostsFile != null);
        message = ''services.luks-ssh-unlocker.instances.${name} cannot set both initrdKnownHosts and initrdKnownHostsFile'';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(instance.notifications.apprise.enable && !instance.notifications.enable);
        message =
          ''services.luks-ssh-unlocker.instances.${name} cannot enable notifications.apprise when notifications.enable is false'';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(instance.notifications.mail.enable && !instance.notifications.enable);
        message =
          ''services.luks-ssh-unlocker.instances.${name} cannot enable notifications.mail when notifications.enable is false'';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(
          instance.initrdCheck.enable
          && instance.initrdCheck.requireSignature
          && instance.initrdCheck.dir == null
        );
        message = ''
          services.luks-ssh-unlocker.instances.${name} sets initrdCheck.requireSignature
          without initrdCheck.dir. fetch_initrd_checksum() only refreshes the signed
          baseline when INITRD_CHECKSUM_DIR is set (via initrdCheck.dir); without it, no
          baseline is ever fetched and, with requireSignature enabled, every unlock
          attempt is silently skipped forever ("file not readable"). Set initrdCheck.dir
          to the directory checksum snapshots should be cached under (a per-hostname
          subdirectory is created automatically).
        '';
      }) cfg.instances);

    systemd.services =
      if cfg.instances == { } then
        { }
      else
        mapAttrs' (
          name: instance:
          nameValuePair "luks-ssh-unlock-${name}" {
            wantedBy = [ "multi-user.target" ];
            after = [ "network.target" ];
            # EnvironmentFile points at a stable /etc path, so the unit
            # text itself never changes when only an instance's options
            # do; without this, switch-to-configuration has nothing to
            # detect and the running process keeps using its stale
            # environment until the next reboot or manual restart.
            restartTriggers = [ config.environment.etc."luks-ssh-unlock/${name}.env".source ];
            serviceConfig = {
              Type = "simple";
              EnvironmentFile = "/etc/luks-ssh-unlock/${name}.env";
              ExecStart = "${package}/bin/luks-ssh-unlock";
            };
          }
        ) cfg.instances;

    system.activationScripts = mkIf cfg.activationScript.enable {
      luksInitrdChecksum = {
        deps = [ "etc" ];
        text = ''
          set -euo pipefail

          CHECKSUM_DIR="${cfg.activationScript.dir}"
          CHECKSUM_FILE="''${CHECKSUM_DIR}/checksum"
          META_FILE="''${CHECKSUM_DIR}/meta.json"

          mkdir -p "$CHECKSUM_DIR"

          # Generate checksum file
          ${package}/bin/initrd-checksum check --initrd="${config.system.build.initialRamdisk}/${config.system.boot.loader.initrdFile}" > "$CHECKSUM_FILE"

          # Sign the checksum baseline with this host's regular SSH host key, so
          # the operator (whose known_hosts already pins that key) can detect a
          # baseline that was tampered with after being fetched off-host.
          rm -f "''${CHECKSUM_FILE}.sig"
          SSH_HOST_KEY="/etc/ssh/ssh_host_ed25519_key"
          if [ -r "$SSH_HOST_KEY" ]
          then
            ${pkgs.openssh}/bin/ssh-keygen -Y sign \
              -f "$SSH_HOST_KEY" \
              -n initrd-checksum \
              "$CHECKSUM_FILE"
          else
            echo "luks-ssh-unlock: warning: $SSH_HOST_KEY not readable, skipping initrd checksum signature" >&2
          fi

          # Build metadata
          GENERATION=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink /nix/var/nix/profiles/system)" | \
            ${pkgs.gnused}/bin/sed 's/^system-\([0-9]\+\)-link$/\1/')
          DATE_STR=$(${pkgs.coreutils}/bin/date -u +"%Y-%m-%dT%H:%M:%SZ")
          CHECKSUM=$(${pkgs.coreutils}/bin/sha256sum "$CHECKSUM_FILE" | ${pkgs.coreutils}/bin/cut -d' ' -f1)

          ${pkgs.jq}/bin/jq -n \
            --argjson generation "$GENERATION" \
            --arg date "$DATE_STR" \
            --arg checksum "$CHECKSUM" \
            '{generation: $generation, date: $date, checksum: $checksum}' > "$META_FILE"
        '';
      };
    };
  };

  options.services.luks-ssh-unlock = {
    enable = mkEnableOption "LUKS SSH Unlock Service";
    activationScript = {
      enable = mkEnableOption "Generate initrd checksum and metadata during activation";
      dir = mkOption {
        type = pathOrStr;
        default = "/etc/initrd-checksum";
        description = "Directory where the activation script stores checksum snapshots and metadata.";
      };
    };
    instances = mkOption {
      default = { };
      type = types.attrsOf (
        types.submodule {
          options = {
            hostname = mkOption {
              type = types.str;
              description = "Hostname of the target machine.";
            };
            username = mkOption {
              type = types.str;
              description = "SSH username for the target machine.";
              default = "root";
            };
            connectionTimeout = mkOption {
              type = types.int;
              description = "SSH connection timeout in seconds.";
              default = 5;
            };
            key = mkOption {
              type = pathOrStr;
              description = "SSH key path for authentication.";
              default = "/etc/ssh/ssh_host_ed25519_key";
            };
            sshKnownHosts = mkOption {
              type = types.str;
              default = "";
              description = "Known hosts entries to validate host keys.";
            };
            sshKnownHostsFile = mkOption {
              type = nullPathOrStr;
              default = null;
              description = "Path to a known hosts file to validate host keys.";
            };
            initrdKnownHosts = mkOption {
              type = types.str;
              default = "";
              description = "Known hosts entries for initrd unlock SSH host keys.";
            };
            initrdKnownHostsFile = mkOption {
              type = nullPathOrStr;
              default = null;
              description = "Path to a known hosts file for initrd unlock SSH host keys.";
            };
            port = mkOption {
              type = types.int;
              default = 22;
              description = "SSH port for the target machine.";
            };
            forceIpv4 = mkOption {
              type = types.bool;
              default = false;
              description = "Force IPv4 for SSH connection.";
            };
            forceIpv6 = mkOption {
              type = types.bool;
              default = false;
              description = "Force IPv6 for SSH connection.";
            };
            type = mkOption {
              type = types.str;
              description = "Type of LUKS operation.";
              default = "systemd";
            };
            passphrase = mkOption {
              type = types.str;
              default = "";
              description = "Passphrase for LUKS.";
            };
            passphraseFile = mkOption {
              type = nullPathOrStr;
              description = "Path to the file containing the passphrase for LUKS.";
              default = null;
            };
            initrdCheck = mkOption {
              type = types.submodule {
                options = {
                  enable = mkEnableOption "Enable initrd integrity validation and snapshot collection.";
                  file = mkOption {
                    type = nullPathOrStr;
                    default = null;
                    description = ''
                      Expected initrd checksum file to validate before unlocking
                      (INITRD_CHECKSUM_FILE). Defaults to
                      "''${dir}/''${hostname}/initrd-checksum/checksum", matching where
                      `dir` (below) fetches the baseline to, so in most setups this does
                      not need to be set explicitly.
                    '';
                  };
                  dir = mkOption {
                    type = nullPathOrStr;
                    default = "/var/lib/luks-ssh-unlock/initrd-checksum";
                    description = ''
                      Directory to store fetched initrd checksum snapshots
                      (INITRD_CHECKSUM_DIR). fetch_initrd_checksum() refreshes the
                      signed baseline here on every successful healthcheck, so leaving
                      this at its default is what makes paranoid/requireSignature mode
                      self-bootstrapping. Set to null to disable baseline fetching
                      entirely (do not combine with requireSignature = true).
                    '';
                  };
                  script = mkOption {
                    type = pathOrStr;
                    # NixOS deploys this via systemd's EnvironmentFile=,
                    # which takes KEY=VALUE lines literally -- it does not
                    # expand $SCRIPT_DIR (that only works when the .env is
                    # shell-sourced, e.g. the Docker entrypoint). Default
                    # to the real store path so INITRD_CHECKSUM_SCRIPT is
                    # actually executable under NixOS, matching busyboxDir
                    # below.
                    default = "${package}/bin/initrd-checksum";
                    description = "Path to initrd-checksum helper (INITRD_CHECKSUM_SCRIPT).";
                  };
                  busyboxDir = mkOption {
                    type = pathOrStr;
                    default = "${package}/busybox";
                    description = "BusyBox bundle directory for paranoid initrd-checksum (INITRD_CHECKSUM_BUSYBOX_DIR).";
                  };
                  paranoid = mkOption {
                    type = types.bool;
                    default = false;
                    description = "Enable paranoid initrd-checksum mode (PARANOID=1).";
                  };
                  requireSignature = mkOption {
                    type = types.bool;
                    default = false;
                    description = ''
                      Fail closed if the fetched checksum baseline has no valid
                      ssh-keygen signature from the target host's regular SSH host key
                      (INITRD_CHECKSUM_REQUIRE_SIGNATURE=1). Only enable this once every
                      target host has redeployed with `activationScript.enable` (so a
                      signed baseline actually exists) and a fresh baseline has been
                      fetched into `dir`.
                    '';
                  };
                };
              };
              default = { };
              description = "Initrd integrity validation settings.";
            };
            eventsFile = mkOption {
              type = nullPathOrStr;
              default = null;
              description = "File path to append events to (EVENTS_FILE).";
            };
            skipSshPortCheck = mkOption {
              type = types.bool;
              default = false;
              description = "Skip preflight SSH port probing (SKIP_SSH_PORT_CHECK=1).";
            };
            tmpdir = mkOption {
              type = nullPathOrStr;
              default = null;
              description = "Working directory for temporary files (TMPDIR).";
            };
            extraEnvironment = mkOption {
              type = types.attrsOf types.str;
              default = { };
              description = "Extra environment variables to add to the EnvironmentFile.";
              example = literalExpression ''{ FOO = "bar"; }'';
            };
            debug = mkOption {
              type = types.bool;
              default = false;
              description = "Enable debug mode.";
            };
            jumpHost = mkOption {
              type = types.nullOr (
                types.submodule {
                  options = {
                    enable = mkEnableOption "Enable jumphost.";
                    hostname = mkOption {
                      type = types.str;
                      description = "Jumphost hostname.";
                    };
                    username = mkOption {
                      type = types.str;
                      description = "Jumphost SSH username.";
                      default = "root";
                    };
                    key = mkOption {
                      type = pathOrStr;
                      description = "Jumphost SSH key path.";
                      default = "/etc/ssh/ssh_host_ed25519_key";
                    };
                    port = mkOption {
                      type = types.int;
                      default = 22;
                      description = "Jumphost SSH port.";
                    };
                  };
                }
              );
              default = null;
              description = "Optional jumphost configuration.";
            };
            sleepInterval = mkOption {
              type = types.int;
              default = 15;
              description = "Time to wait between attempts.";
            };
            tickTimeout = mkOption {
              type = types.int;
              default = 120;
              description = "Timeout for each tick.";
            };
            healthcheck = mkOption {
              type = types.submodule {
                options = {
                  enable = mkEnableOption "Healthcheck on/off.";
                  port = mkOption {
                    type = types.nullOr types.int;
                    description = "Health check port.";
                    default = null;
                  };
                  hostname = mkOption {
                    type = types.str;
                    description = "Remote hostname to run the command on.";
                    default = "";
                  };
                  username = mkOption {
                    type = types.str;
                    description = "Remote username for the remote healthcheck command.";
                    default = "";
                  };
                  command = mkOption {
                    type = types.str;
                    description = "Remote command to verify the status.";
                    default = "";
                  };
                  knownHostsType = mkOption {
                    type = types.enum [
                      "default"
                      "initrd"
                    ];
                    description = "Which known_hosts set to use for health-check SSH (SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE).";
                    default = "default";
                  };
                };
              };
              default = { };
              description = "Health check configuration.";
            };
              notifications = mkOption {
                type = types.submodule {
                  options = {
                    enable = mkEnableOption "Enable notifications.";
                    apprise = mkOption {
                      type = types.submodule {
                        options = {
                          enable = mkEnableOption "Enable Apprise notifications.";
                          url = mkOption {
                            type = types.str;
                            default = "";
                            description = "Apprise URL (APPRISE_URL).";
                          };
                          tag = mkOption {
                            type = types.str;
                            default = "";
                            description = "Apprise tag (APPRISE_TAG).";
                          };
                          title = mkOption {
                            type = types.str;
                            default = "";
                            description = "Apprise title (APPRISE_TITLE).";
                          };
                        };
                      };
                      default = { };
                      description = "Apprise settings.";
                    };
                    msmtp = mkOption {
                      type = types.submodule {
                        options = {
                          account = mkOption {
                            type = types.str;
                            default = "";
                            description = "msmtp account to use (MSMTP_ACCOUNT).";
                          };
                        };
                      };
                      default = { };
                      description = "msmtp settings.";
                    };
                    mail = mkOption {
                      type = types.submodule {
                        options = {
                          enable = mkEnableOption "Enable email notifications.";
                          recipient = mkOption {
                            type = types.str;
                            default = "";
                            description = "Email recipient address (EMAIL_RECIPIENT).";
                          };
                          from = mkOption {
                            type = types.str;
                            default = "";
                            description = "Email sender address (EMAIL_FROM).";
                          };
                          subject = mkOption {
                            type = types.str;
                            default = "";
                            description = "Email subject (EMAIL_SUBJECT).";
                          };
                        };
                      };
                      default = { };
                      description = "Email settings.";
                    };
                  };
                };
                default = { };
                description = "Notification configuration.";
              };
          };
        }
      );
      description = "Configuration for multiple LUKS SSH Unlocker instances.";
    };
  };
}
