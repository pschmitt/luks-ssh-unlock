{
  config,
  lib,
  pkgs,
  ...
}:

with lib;

let
  cfg = config.services.luks-ssh-unlock;
  package = pkgs.callPackage ./package.nix {
    busyboxBundle = pkgs.callPackage ./busybox.nix {
      busyboxAmd64 = pkgs.pkgsStatic.busybox;
      busyboxArm64 = pkgs.pkgsStatic.busybox;
    };
  };
in
{
  options.services.luks-ssh-unlock = {
    enable = mkEnableOption "LUKS SSH Unlock Service";
    activationScript = {
      enable = mkEnableOption "Generate initrd checksum and metadata during activation";
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
            key = mkOption {
              type = types.path;
              description = "SSH key path for authentication.";
              default = "/etc/ssh/ssh_host_ed25519_key";
            };
            sshKnownHosts = mkOption {
              type = types.str;
              default = "";
              description = "Known hosts entries to validate host keys.";
            };
            sshKnownHostsFile = mkOption {
              type = types.nullOr types.path;
              default = null;
              description = "Path to a known hosts file to validate host keys.";
            };
            initrdKnownHosts = mkOption {
              type = types.str;
              default = "";
              description = "Known hosts entries for initrd unlock SSH host keys.";
            };
            initrdKnownHostsFile = mkOption {
              type = types.nullOr types.path;
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
              type = types.path;
              description = "Path to the file containing the passphrase for LUKS.";
              default = "";
            };
            initrdChecksumFile = mkOption {
              type = types.nullOr types.path;
              default = null;
              description = "Path to an initrd checksum file; enables checksum validation before unlocking when set.";
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
                      type = types.path;
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
                };
              };
              description = "Health check configuration.";
            };
            emailNotifications = mkOption {
              type = types.submodule {
                options = {
                  enable = mkEnableOption "Enable email notifications.";
                  recipient = mkOption {
                    type = types.str;
                    description = "Email recipient address.";
                    default = "";
                  };
                  sender = mkOption {
                    type = types.str;
                    description = "Email sender address.";
                    default = "";
                  };
                  subject = mkOption {
                    type = types.str;
                    description = "Email subject (supports templating).";
                    default = "";
                  };
                };
              };
              description = "Email notifications";
            };
          };
        }
      );
      description = "Configuration for multiple LUKS SSH Unlocker instances.";
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [
      package
    ];

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
                  sshKnownHostsFile
                else
                  "";
              initrdKnownHostsPath =
                if initrdKnownHosts != "" then
                  "/etc/luks-ssh-unlock/${name}.initrd_known_hosts"
                else if initrdKnownHostsFile != null then
                  initrdKnownHostsFile
                else
                  "";
            in
            ''
              DEBUG=${optionalString debug "1"}
              SLEEP_INTERVAL=${toString sleepInterval}

              SSH_HOSTNAME=${hostname}
              SSH_USER=${username}
              SSH_KEY=${key}
              SSH_PORT=${toString port}

              FORCE_IPV4=${optionalString forceIpv4 "1"}
              FORCE_IPV6=${optionalString forceIpv6 "1"}

              ${optionalString (knownHostsPath != "") ''
                SSH_KNOWN_HOSTS_FILE=${knownHostsPath}
              ''}
              ${optionalString (initrdKnownHostsPath != "") ''
                SSH_INITRD_KNOWN_HOSTS_FILE=${initrdKnownHostsPath}
              ''}

              ${optionalString instance.jumpHost.enable ''
                SSH_JUMPHOST=${optionalString (jumpHost.hostname != null) jumpHost.hostname}
                SSH_JUMPHOST_USERNAME=${optionalString (jumpHost.username != null) jumpHost.username}
                SSH_JUMPHOST_PORT=${optionalString (jumpHost.port != null) (toString jumpHost.port)}
                SSH_JUMPHOST_KEY=${optionalString (jumpHost.key != null) jumpHost.key}
              ''}

              LUKS_PASSPHRASE=${passphrase}
              LUKS_PASSPHRASE_FILE=${passphraseFile}
              LUKS_TYPE=${type}

              ${optionalString (initrdChecksumFile != null) ''
                INITRD_CHECKSUM_FILE=${initrdChecksumFile}
              ''}

              ${optionalString instance.healthcheck.enable ''
                HEALTHCHECK_PORT=${optionalString (healthcheck.port != null) (toString healthcheck.port)}
                HEALTHCHECK_REMOTE_HOSTNAME="${optionalString (healthcheck.hostname != "") healthcheck.hostname}"
                HEALTHCHECK_REMOTE_USERNAME="${optionalString (healthcheck.username != "") healthcheck.username}"
                HEALTHCHECK_REMOTE_CMD="${healthcheck.command}"
              ''}

              ${optionalString instance.emailNotifications.enable ''
                EMAIL_RECIPIENT="${
                  optionalString (emailNotifications.recipient != "") emailNotifications.recipient
                }"
                EMAIL_SENDER="${optionalString (emailNotifications.sender != "") emailNotifications.sender}"
                EMAIL_SUBJECT="${optionalString (emailNotifications.subject != "") emailNotifications.subject}"
              ''}
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

          CHECKSUM_DIR="/etc/initrd-checksum"
          CHECKSUM_FILE="''${CHECKSUM_DIR}/checksum"
          META_FILE="''${CHECKSUM_DIR}/meta.json"

          mkdir -p "$CHECKSUM_DIR"

          # Generate checksum file
          ${package}/bin/initrd-checksum --initrd="${config.system.build.initialRamdisk}/${config.system.boot.loader.initrdFile}" > "$CHECKSUM_FILE"

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
}
