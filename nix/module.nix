{
  config,
  lib,
  pkgs,
  ...
}:

with lib;

let
  cfg = config.services.luks-ssh-unlock;
  escapeEnvironmentFileValue = value: replaceStrings [ "\\" "\"" ] [ "\\\\" "\\\"" ] value;
  passphraseCredential =
    instance:
    optional (instance.passphraseFile != null) "luks-passphrase:${toString instance.passphraseFile}";
  pathOrStr = types.either types.path types.str;
  nullPathOrStr = types.nullOr pathOrStr;
  inherit (cfg) package;
  # Settings of one graphical source that the others' files must not carry
  # (the file rejects settings it does not know).
  openstackOnly = [
    "server"
    "cloud"
    "cloudsFile"
    "caFile"
    "allowInsecure"
    "openstackBin"
  ];
  ociOnly = [
    "instance"
    "identity"
    "configFile"
    "cliRcFile"
    "profile"
    "compartmentId"
    "keep"
  ];
  adHocUnlockers = mapAttrsToList (
    name: instance:
    pkgs.writeShellApplication {
      name = "luks-ssh-unlock-${name}";
      runtimeInputs = [
        pkgs.coreutils
        pkgs.systemd
      ];
      text = ''
        # The dashboard acts as the invoking user and never runs as root: what
        # needs root goes through the privileged helper.
        case "''${1:-}" in
          tui|ui)
            shift
            exec ${package}/bin/luks-ssh-unlock tui "$@" /etc/luks-ssh-unlock/${name}.env
            ;;
        esac

        if [[ "$EUID" -ne 0 ]]
        then
          exec /run/wrappers/bin/sudo -- "$0" "$@"
        fi

        credential_properties=()
        ${optionalString (instance.passphraseFile != null) ''
          credential_properties+=(--property=${escapeShellArg "LoadCredential=luks-passphrase:${toString instance.passphraseFile}"})
        ''}

        case "''${1:-}" in
          logs|l)
            shift
            exec journalctl \
              --follow \
              --unit=luks-ssh-unlock-${name}.service \
              "$@" \
              --output=cat
            ;;
          start|stop)
            if [[ $# -ne 1 ]]
            then
              printf 'Usage: luks-ssh-unlock-${name} start|stop\n' >&2
              exit 2
            fi
            exec systemctl "$1" luks-ssh-unlock-${name}.service
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
              --quiet \
              --wait \
              --pipe \
              --collect \
              --unit=${escapeShellArg "luks-ssh-unlock-status-${name}-"}"$$" \
              --property=${escapeShellArg "EnvironmentFile=/etc/luks-ssh-unlock/${name}.env"} \
              "''${status_environment[@]}" \
              -- ${package}/bin/luks-ssh-unlock status || status_exit=$?
            service_state=$(systemctl is-active luks-ssh-unlock-${name}.service 2>/dev/null) || :
            service_label=$service_state
            if [[ "$service_state" == inactive ]]
            then
              service_label='stopped (inactive)'
            elif [[ -z "$service_label" ]]
            then
              service_label=unknown
            fi
            service_color=
            service_reset=
            if [[ -n "''${FORCE_COLOR:-}" || ( -t 1 && "''${TERM:-}" != dumb ) ]]
            then
              service_reset=$'\033[0m'
              if [[ "$service_state" == active ]]
              then
                service_color=$'\033[32m'
              else
                service_color=$'\033[1;31m'
              fi
            fi
            printf '\nUnlock service: %s%s%s\n' "$service_color" "$service_label" "$service_reset"
            last_unlock=$(journalctl \
              --quiet \
              --grep=${escapeShellArg "^Unlocked ${instance.hostname}; "} \
              --no-pager \
              --output=short-iso \
              -n 1) || last_unlock=
            if [[ -n "$last_unlock" ]]
            then
              last_unlock=$(LC_ALL=C date --date="''${last_unlock%% *}" '+%d %b %Y, %H:%M:%S %Z')
              printf '\nLast successful unlock: %s\n' "$last_unlock"
            else
              printf '\nLast successful unlock: none recorded\n'
            fi
            printf '\nRecent daemon logs for ${name}:\n'
            log_lines=$(journalctl _SYSTEMD_UNIT=luks-ssh-unlock-${name}.service --no-pager --output=cat -n 5) || log_lines=
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
          --quiet \
          --wait \
          --pipe \
          --collect \
          --unit=${escapeShellArg "luks-ssh-unlock-manual-${name}-"}"$$" \
          --property=${escapeShellArg "EnvironmentFile=/etc/luks-ssh-unlock/${name}.env"} \
          --property=TimeoutStartSec=infinity \
          "''${credential_properties[@]}" \
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
              initrdChecksumDir = if initrdCheck.dir != null then toString initrdCheck.dir else null;
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
                  EMAIL_RECIPIENT="${
                    optionalString (notifications.mail.recipient != "") notifications.mail.recipient
                  }"
                  EMAIL_FROM="${optionalString (notifications.mail.from != "") notifications.mail.from}"
                  EMAIL_SUBJECT=${escapeShellArg notifications.mail.subject}
                ''}
              ''}

              ${optionalString healthcheck.enable ''
                HEALTHCHECK_PORT=${optionalString (healthcheck.port != null) (toString healthcheck.port)}
                HEALTHCHECK_REMOTE_HOSTNAME="${optionalString (healthcheck.hostname != "") healthcheck.hostname}"
                HEALTHCHECK_REMOTE_USERNAME="${optionalString (healthcheck.username != "") healthcheck.username}"
                HEALTHCHECK_REMOTE_CMD="${escapeEnvironmentFileValue healthcheck.command}"
                SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE=${healthcheck.knownHostsType}
              ''}

              ${concatStringsSep "\n" (mapAttrsToList (k: v: "${k}=${escapeShellArg v}") extraEnvironment)}
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
      ) (filterAttrs (_: instance: instance.initrdKnownHosts != "") cfg.instances))
      // (mapAttrs' (
        name: instance:
        nameValuePair "luks-ssh-unlock/${name}.tui-commands" {
          text = concatStringsSep "\n" instance.tuiCommands + "\n";
        }
      ) (filterAttrs (_: instance: instance.tuiCommands != [ ]) cfg.instances))
      // (mapAttrs' (
        name: instance:
        nameValuePair "luks-ssh-unlock/${name}.console.json" {
          text = builtins.toJSON (
            instance.console // optionalAttrs (instance.defaultView == "console") { default = true; }
          );
        }
      ) (filterAttrs (_: instance: instance.console != null) cfg.instances))
      // (mapAttrs' (
        name: instance:
        nameValuePair "luks-ssh-unlock/${name}.vnc.json" {
          text = builtins.toJSON {
            inherit (instance.vnc) type;
            default = instance.defaultView == "vnc";
            # Each source takes only its own settings (the file rejects others).
            parameters = filterAttrs (_: value: value != null) (
              removeAttrs instance.vnc (
                [ "type" ]
                ++ (
                  if instance.vnc.type == "openstack" then
                    [
                      "host"
                      "port"
                      "allowRemote"
                    ]
                    ++ ociOnly
                  else if instance.vnc.type == "oci" then
                    [
                      "host"
                      "port"
                      "allowRemote"
                    ]
                    ++ openstackOnly
                  else
                    openstackOnly ++ ociOnly
                )
              )
            );
          };
        }
      ) (filterAttrs (_: instance: instance.vnc != null) cfg.instances))
      // optionalAttrs (cfg.tui.extraCommands != [ ]) {
        "luks-ssh-unlock/tui-commands".text = concatStringsSep "\n" cfg.tui.extraCommands + "\n";
      };

    warnings = concatLists (
      mapAttrsToList (
        name: instance:
        optional (instance.passphrase != "") ''
          services.luks-ssh-unlock.instances.${name}.passphrase is set: the LUKS
          passphrase is written in plain text to an EnvironmentFile in the
          world-readable Nix store (and into every system closure that contains it).
          Use passphraseFile with a secret path (e.g. sops-nix or agenix) instead;
          it is passed to the service as a systemd credential.
        ''
      ) cfg.instances
    );

    assertions =
      (mapAttrsToList (name: instance: {
        assertion = !(instance.sshKnownHosts != "" && instance.sshKnownHostsFile != null);
        message = "services.luks-ssh-unlocker.instances.${name} cannot set both sshKnownHosts and sshKnownHostsFile";
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(instance.initrdKnownHosts != "" && instance.initrdKnownHostsFile != null);
        message = "services.luks-ssh-unlocker.instances.${name} cannot set both initrdKnownHosts and initrdKnownHostsFile";
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(instance.notifications.apprise.enable && !instance.notifications.enable);
        message = "services.luks-ssh-unlocker.instances.${name} cannot enable notifications.apprise when notifications.enable is false";
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion = !(instance.notifications.mail.enable && !instance.notifications.enable);
        message = "services.luks-ssh-unlocker.instances.${name} cannot enable notifications.mail when notifications.enable is false";
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion =
          !(
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
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion =
          !instance.dhcpListener.enable
          || (instance.dhcpListener.interface != "" && instance.dhcpListener.clientHostname != "");
        message = ''
          services.luks-ssh-unlock.instances.${name}.dhcpListener.enable requires
          both interface and clientHostname.
        '';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion =
          instance.vnc == null
          || instance.vnc.passwordFile == null
          || (
            hasPrefix "/" instance.vnc.passwordFile && !hasPrefix builtins.storeDir instance.vnc.passwordFile
          );
        message = ''
          services.luks-ssh-unlock.instances.${name}.vnc.passwordFile must be an absolute
          runtime path outside the Nix store (the store is world-readable).
        '';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion =
          instance.vnc == null
          || instance.vnc.type != "vnc"
          || instance.vnc.allowRemote
          || instance.vnc.host == "127.0.0.1"
          || instance.vnc.host == "::1"
          || instance.vnc.host == "localhost";
        message = ''
          services.luks-ssh-unlock.instances.${name}.vnc.host is not a loopback address:
          raw VNC is not encrypted. Tunnel it to a loopback port, or set
          vnc.allowRemote = true to accept the risk.
        '';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion =
          instance.vnc == null
          || instance.vnc.type != "openstack"
          || (
            all (path: path == null || hasPrefix "/" path) [
              instance.vnc.cloudsFile
              instance.vnc.caFile
              instance.vnc.openstackBin
            ]
            && (instance.vnc.cloudsFile == null || !hasPrefix builtins.storeDir instance.vnc.cloudsFile)
          );
        message = ''
          services.luks-ssh-unlock.instances.${name}.vnc: cloudsFile, caFile and
          openstackBin must be absolute paths, and cloudsFile (it holds
          credentials) must be a runtime path outside the Nix store.
        '';
      }) cfg.instances)
      ++ (mapAttrsToList (name: instance: {
        assertion =
          instance.vnc == null
          || instance.vnc.type != "oci"
          || (
            all (path: path == null || hasPrefix "/" path) [
              instance.vnc.configFile
              instance.vnc.cliRcFile
            ]
            && (
              instance.vnc.identity == null
              || (
                (hasPrefix "/" instance.vnc.identity || hasPrefix "~/" instance.vnc.identity)
                && !hasPrefix builtins.storeDir instance.vnc.identity
              )
            )
          );
        message = ''
          services.luks-ssh-unlock.instances.${name}.vnc: configFile and cliRcFile
          must be absolute paths, and identity (a private key) must be an
          absolute or ~/ runtime path outside the Nix store.
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
              LoadCredential = passphraseCredential instance;
              ExecStart = "${package}/bin/luks-ssh-unlock";
            };
          }
        ) cfg.instances
        // mapAttrs' (
          name: instance:
          nameValuePair "luks-ssh-unlock-dhcp-listener-${name}" {
            wantedBy = [ "multi-user.target" ];
            after = [ "network.target" ];
            serviceConfig = {
              Type = "simple";
              Restart = "always";
              RestartSec = 5;
              AmbientCapabilities = [ "CAP_NET_RAW" ];
              CapabilityBoundingSet = [ "CAP_NET_RAW" ];
              LoadCredential = passphraseCredential instance;
              ExecStart =
                "${package}/bin/luks-ssh-unlock-dhcp-listener "
                + escapeShellArgs [
                  "--interface"
                  instance.dhcpListener.interface
                  "--client-hostname"
                  instance.dhcpListener.clientHostname
                  "--target-hostname"
                  instance.hostname
                  "--ssh-port"
                  (toString instance.port)
                  "--ssh-wait-timeout"
                  (toString instance.dhcpListener.sshWaitTimeout)
                  "--environment-file"
                  "/etc/luks-ssh-unlock/${name}.env"
                  "--unlocker"
                  "${package}/bin/luks-ssh-unlock"
                ];
            };
          }
        ) (filterAttrs (_: instance: instance.dhcpListener.enable) cfg.instances);

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
    package = mkOption {
      type = types.package;
      default =
        pkgs.luks-ssh-unlock
          or (throw "services.luks-ssh-unlock.package is not set: import the module through the luks-ssh-unlock flake (nixosModules.default) or add its overlay");
      defaultText = literalExpression "the luks-ssh-unlock flake's package";
      description = "luks-ssh-unlock package to run.";
    };
    tui.extraCommands = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = ''
        Commands whose output the dashboard (`luks-ssh-unlock tui`) shows for
        the selected target, in an extra pane below the activity. Each is
        `[LABEL=]COMMAND`, run with `sh -c`; `{host}` and `{name}` are replaced
        by the target's host and name. Written to
        `/etc/luks-ssh-unlock/tui-commands`.
      '';
      example = literalExpression ''[ "console=zsh -lc 'zhj openstack::console-log {host}'" ]'';
    };
    tui.defaultView = mkOption {
      type = types.nullOr (
        types.enum [
          "console"
          "vnc"
        ]
      );
      default = null;
      example = "vnc";
      description = ''
        The view the dashboard shows first for a target that has it: its
        `console` or its graphical `vnc` console. `null` keeps the order of the
        extra commands. Each instance can override it with `defaultView`.
      '';
    };
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
              description = ''
                Passphrase for LUKS. Insecure: it ends up in plain text in the
                world-readable Nix store. Prefer {option}`passphraseFile`.
              '';
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
            dhcpListener = mkOption {
              type = types.submodule {
                options = {
                  enable = mkEnableOption "Unlock this target after its DHCP lease becomes reachable.";
                  interface = mkOption {
                    type = types.str;
                    default = "";
                    description = "Interface on which to listen for DHCP ACK packets, or any for all interfaces.";
                  };
                  clientHostname = mkOption {
                    type = types.str;
                    default = "";
                    description = "DHCP option 12 hostname used to identify the target.";
                  };
                  sshWaitTimeout = mkOption {
                    type = types.ints.positive;
                    default = 180;
                    description = "How long to wait for SSH after DHCP assigns an address.";
                  };
                };
              };
              default = { };
              description = "Optional DHCP-triggered unlock listener settings.";
            };
            defaultView = mkOption {
              type = types.nullOr (
                types.enum [
                  "console"
                  "vnc"
                ]
              );
              default = cfg.tui.defaultView;
              defaultText = literalExpression "config.services.luks-ssh-unlock.tui.defaultView";
              description = "The view the dashboard shows first for this target (see `tui.defaultView`).";
            };
            console = mkOption {
              type = types.nullOr (
                types.submodule (
                  { config, ... }:
                  {
                    options = {
                      type = mkOption {
                        type = types.enum [
                          "command"
                          "oci"
                          "openstack"
                        ];
                        description = ''
                          Console provider. `openstack` is the serial console of an
                          OpenStack server (the `openstack` CLI must be in the console
                          user's PATH, or set `openstackBin`).
                        '';
                      };
                      parameters = mkOption {
                        type =
                          if config.type == "command" then
                            types.submodule {
                              options = {
                                command = mkOption { type = types.str; };
                                interactive = mkOption {
                                  type = types.bool;
                                  default = false;
                                };
                              };
                            }
                          else if config.type == "oci" then
                            types.submodule {
                              options = {
                                instance = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                identity = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                configFile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                cliRcFile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                profile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                compartmentId = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                keep = mkOption {
                                  type = types.bool;
                                  default = false;
                                };
                              };
                            }
                          else if config.type == "openstack" then
                            types.submodule {
                              options = {
                                server = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                  description = "Server name or ID. Defaults to the target name.";
                                };
                                cloud = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                  description = "Cloud name in clouds.yaml (--os-cloud).";
                                };
                                cloudsFile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                  description = "Runtime path of clouds.yaml (never a Nix store path).";
                                };
                                caFile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                  description = "CA bundle for the console proxy (wss).";
                                };
                                allowInsecure = mkOption {
                                  type = types.bool;
                                  default = false;
                                  description = "Accept a plain ws:// console proxy on a non-loopback host.";
                                };
                                openstackBin = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                  description = "Absolute path of the openstack CLI.";
                                };
                              };
                            }
                          else
                            types.attrsOf types.anything;
                        default = { };
                        description = "Provider-specific console parameters. Paths must be runtime path strings, not Nix paths.";
                      };
                    };
                  }
                )
              );
              default = null;
              description = "Typed dashboard console configuration for this target.";
              example = literalExpression ''{ type = "oci"; parameters.identity = "/home/user/.ssh/id_rsa"; }'';
            };
            vnc = mkOption {
              type = types.nullOr (
                types.submodule {
                  options = {
                    type = mkOption {
                      type = types.enum [
                        "vnc"
                        "openstack"
                        "oci"
                      ];
                      default = "vnc";
                      description = ''
                        Where the desktop comes from: `vnc` is a direct VNC endpoint
                        (`host`, `port`); `openstack` is the noVNC console of an
                        OpenStack server, reached through the `openstack` CLI and
                        its console proxy (`server`, `cloud`, ...); `oci` is the
                        graphical console of an OCI instance, reached through the
                        `oci` CLI and an `ssh` tunnel to a private Unix socket
                        (`instance`, `identity`, `profile`, ...). OCI allows one
                        console client at a time.
                      '';
                    };
                    instance = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "OCI instance display name or OCID (type oci). Defaults to the target name.";
                    };
                    identity = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = ''
                        Runtime path of the RSA private key whose `.pub` file is
                        registered with the console connection (type oci;
                        `~/.ssh/id_rsa` by default). Never a Nix store path.
                      '';
                    };
                    configFile = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "Absolute path of the OCI CLI config, --config-file (type oci).";
                    };
                    cliRcFile = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "Absolute path of the OCI CLI rc file, --cli-rc-file (type oci).";
                    };
                    profile = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "OCI CLI profile, --profile (type oci).";
                    };
                    compartmentId = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "Compartment OCID for the lookup of the instance by name (type oci).";
                    };
                    keep = mkOption {
                      type = types.bool;
                      default = false;
                      description = "Keep the console connection at the provider on exit (type oci).";
                    };
                    server = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "OpenStack server name or ID (type openstack). Defaults to the target name.";
                    };
                    cloud = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "OpenStack cloud name in clouds.yaml, --os-cloud (type openstack).";
                    };
                    cloudsFile = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = ''
                        Runtime path of clouds.yaml (type openstack). It holds
                        credentials: never a Nix store path.
                      '';
                    };
                    caFile = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "CA bundle for the console proxy (type openstack).";
                    };
                    allowInsecure = mkOption {
                      type = types.bool;
                      default = false;
                      description = ''
                        Accept a plain ws:// console proxy on a non-loopback host
                        (type openstack). The pane is then marked UNENCRYPTED.
                      '';
                    };
                    openstackBin = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = "Absolute path of the openstack CLI (type openstack).";
                    };
                    host = mkOption {
                      type = types.str;
                      default = "127.0.0.1";
                      description = ''
                        VNC server host. Raw VNC is not encrypted: keep this on
                        loopback (for example the local end of an SSH tunnel).
                      '';
                    };
                    port = mkOption {
                      type = types.port;
                      default = 5900;
                      description = "VNC server port.";
                    };
                    passwordFile = mkOption {
                      type = types.nullOr types.str;
                      default = null;
                      description = ''
                        Runtime path of a file holding the VNC password (mode 0600,
                        owned by root or the invoking user). It is read by the
                        dashboard when a session starts and is never copied into
                        the Nix store or put on a command line. Must not be a Nix
                        store path.
                      '';
                      example = "/run/secrets/vnc-password";
                    };
                    viewOnly = mkOption {
                      type = types.bool;
                      default = false;
                      description = "Start without forwarding the keyboard and mouse (Ctrl+T toggles).";
                    };
                    quality = mkOption {
                      type = types.nullOr (types.ints.between 0 9);
                      default = null;
                      description = "Tight JPEG quality, 0 to 9.";
                    };
                    compression = mkOption {
                      type = types.nullOr (types.ints.between 0 9);
                      default = null;
                      description = "Tight compression level, 0 to 9.";
                    };
                    resizeRemote = mkOption {
                      type = types.bool;
                      default = false;
                      description = "Ask the remote desktop to take the size of the dashboard pane.";
                    };
                    keymap = mkOption {
                      type = types.nullOr (
                        types.enum [
                          "de"
                          "us"
                        ]
                      );
                      default = null;
                      description = ''
                        Keyboard layout of the guest's console. Keys are sent as the
                        scancodes that type each character in this layout (when the
                        server takes scancodes, as QEMU does), so the guest gets what
                        was typed. `null` assumes the dashboard machine's own console
                        layout (`KEYMAP` in `/etc/vconsole.conf`).
                      '';
                    };
                    allowRemote = mkOption {
                      type = types.bool;
                      default = false;
                      description = ''
                        Accept a host that is not a loopback address. The
                        connection is then unencrypted and the dashboard says so.
                      '';
                    };
                  };
                }
              );
              default = null;
              description = ''
                Graphical console of this target, shown by the dashboard's
                built-in viewer next to the serial console: a direct VNC endpoint
                the noVNC console of an OpenStack server (`type = "openstack"`) or
                the graphical console of an OCI instance (`type = "oci"`).
                It does not depend on the console provider.
              '';
              example = literalExpression ''{ port = 5901; passwordFile = "/run/secrets/vnc"; }'';
            };
            tuiCommands = mkOption {
              type = types.listOf types.str;
              default = [ ];
              description = ''
                Dashboard commands for this target only, `[LABEL=]COMMAND`: they
                replace the common `services.luks-ssh-unlock.tui.extraCommands`
                of the same label, add new ones, and an empty command
                (`"console="`) removes that label for this target.
              '';
              example = literalExpression ''[ "console=ipmi-console {host}" ]'';
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
