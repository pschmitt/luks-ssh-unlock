{
  config,
  lib,
  ...
}:

let
  inherit (lib)
    genAttrs
    mapAttrs
    mapAttrs'
    mkDefault
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    types
    ;
  cfg = config.services.luks-ssh-unlock;
  inherit (cfg) helper package;

  basicGroup = "luks-ssh-unlock";
  adminGroup = "luks-ssh-unlock-admin";
  socketDir = "/run/luks-ssh-unlock";

  # Which socket a connection arrives on decides what it may do; nothing in a
  # request can change that. The sockets are closed to everyone but root and
  # the group, and the helper itself refuses to serve on one that is not.
  policies = {
    basic = {
      group = basicGroup;
      description = "list, check and unlock";
    };
    admin = {
      group = adminGroup;
      description = "list, check, unlock, forced unlock and service start/stop";
    };
  };

  hardening = {
    User = "root";
    StandardInput = "socket";
    StandardOutput = "journal";
    StandardError = "journal";
    RuntimeDirectory = "luks-ssh-unlock-helper/%i";
    RuntimeDirectoryMode = "0700";
    # A stuck connection cannot hold a process for ever.
    RuntimeMaxSec = "1h";
    UMask = "0077";
    NoNewPrivileges = true;
    CapabilityBoundingSet = [ "" ];
    PrivateTmp = true;
    PrivateDevices = true;
    ProtectSystem = "strict";
    # Keys under /root or /home stay readable for ssh, never writable.
    ProtectHome = "read-only";
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    ProtectProc = "invisible";
    ProcSubset = "pid";
    RestrictAddressFamilies = [
      "AF_UNIX"
      "AF_INET"
      "AF_INET6"
    ];
    RestrictNamespaces = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    RemoveIPC = true;
    SystemCallArchitectures = "native";
  };

  mkSocket = policy: {
    wantedBy = [ "sockets.target" ];
    socketConfig = {
      ListenStream = "${socketDir}/${policy}.sock";
      Accept = true;
      SocketUser = "root";
      SocketGroup = policies.${policy}.group;
      SocketMode = "0660";
      DirectoryMode = "0755";
      RemoveOnStop = true;
      MaxConnections = 16;
      MaxConnectionsPerSource = 8;
    };
  };

  mkService = policy: {
    description = "luks-ssh-unlock privileged helper (${policies.${policy}.description})";
    serviceConfig =
      (mapAttrs (_: mkDefault) hardening)
      // {
        ExecStart =
          "${package}/bin/luks-ssh-unlock helper serve --policy ${policy} "
          + "--checksum-script ${package}/bin/initrd-checksum";
      }
      // helper.serviceConfig;
  };
in
{
  options.services.luks-ssh-unlock.helper = {
    enable = mkEnableOption ''
      the privileged helper behind `luks-ssh-unlock tui`.

      The dashboard runs as the invoking user, with that user's own keys and
      environment (provider consoles included). For targets whose
      configuration, SSH key or passphrase only root can read, it asks this
      helper, which answers with closed states and never returns a secret.
      The helper listens on root-owned sockets that only the groups below can
      reach; it is started per connection by systemd'';
    users = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "alice" ];
      description = ''
        Users added to the `${basicGroup}` group: they may list, check and
        unlock targets through the helper. Only targets that have a checksum
        baseline and pinned initrd host keys can be unlocked this way.
      '';
    };
    adminUsers = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "alice" ];
      description = ''
        Users added to the `${adminGroup}` group (and to `${basicGroup}`): they
        may also force an unlock (skipping the integrity and host-key checks)
        and start or stop a target's unit. Treat membership like sudo access to
        those actions.
      '';
    };
    protectConfig = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Make the environment files in `/etc/luks-ssh-unlock` readable by root
        only, so that the dashboard can learn about a target (its host, its
        checks, its state) only through the helper.
      '';
    };
    serviceConfig = mkOption {
      type = types.attrs;
      default = { };
      description = ''
        Overrides for the `serviceConfig` of both helper services, for example
        to relax a sandboxing option on a host where it prevents a key from
        being read.
      '';
    };
  };

  config = mkIf (cfg.enable && helper.enable) (mkMerge [
    {
      users = {
        groups.${basicGroup} = { };
        groups.${adminGroup} = { };
        users = mkMerge [
          (genAttrs helper.users (_: {
            extraGroups = [ basicGroup ];
          }))
          (genAttrs helper.adminUsers (_: {
            extraGroups = [
              basicGroup
              adminGroup
            ];
          }))
        ];
      };

      systemd.sockets = {
        luks-ssh-unlock-helper-basic = mkSocket "basic";
        luks-ssh-unlock-helper-admin = mkSocket "admin";
      };
      systemd.services = {
        "luks-ssh-unlock-helper-basic@" = mkService "basic";
        "luks-ssh-unlock-helper-admin@" = mkService "admin";
      };
    }
    (mkIf helper.protectConfig {
      environment.etc = mapAttrs' (name: _: {
        name = "luks-ssh-unlock/${name}.env";
        value.mode = "0600";
      }) cfg.instances;
    })
  ]);
}
