{
  description = "Unlock LUKS-encrypted hosts remotely over SSH";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Builds the dependencies in their own derivation, so CI caches and hosts
    # reuse them instead of recompiling every crate on each change.
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
    }:
    let
      # The target's busybox bundle holds a real binary per target arch, from
      # a native package set so it comes from the binary cache.
      busyboxFor =
        pkgs:
        pkgs.callPackage ./nix/busybox.nix {
          busyboxAmd64 = nixpkgs.legacyPackages.x86_64-linux.pkgsStatic.busybox;
          busyboxArm64 = nixpkgs.legacyPackages.aarch64-linux.pkgsStatic.busybox;
        };
      packageFor =
        pkgs:
        pkgs.callPackage ./nix/package.nix {
          craneLib = crane.mkLib pkgs;
          busyboxBundle = busyboxFor pkgs;
        };
      eachSystem = flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          luks-ssh-unlock = packageFor pkgs;
        in
        {
          packages = {
            default = luks-ssh-unlock;
            inherit luks-ssh-unlock;
            busybox-static = busyboxFor pkgs;
            entrypoint = pkgs.callPackage ./nix/entrypoint.nix { };
          };

          apps.default = {
            type = "app";
            program = "${luks-ssh-unlock}/bin/luks-ssh-unlock";
          };

          # The package build runs the unit tests.
          checks = {
            inherit luks-ssh-unlock;
            # Evaluates a NixOS system with one instance and checks the
            # rendered units point at this flake's package.
            nixos-module =
              let
                nixos = nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    # Importing it twice (nixos-config does) must not define the package twice.
                    self.nixosModules.default
                    {
                      services.luks-ssh-unlock = {
                        enable = true;
                        helper = {
                          enable = true;
                          users = [ "alice" ];
                          adminUsers = [ "bob" ];
                          protectConfig = true;
                        };
                        instances.smoke = {
                          hostname = "target.example.invalid";
                          key = "/run/secrets/ssh_key";
                          passphraseFile = "/run/secrets/luks";
                          console = {
                            type = "oci";
                            parameters.identity = "/home/user/.ssh/id_rsa";
                          };
                          vnc = {
                            port = 5901;
                            passwordFile = "/run/secrets/vnc";
                            viewOnly = true;
                          };
                          dhcpListener = {
                            enable = true;
                            interface = "any";
                            clientHostname = "target";
                          };
                        };
                      };
                      boot.loader.grub.enable = false;
                      fileSystems."/" = {
                        device = "none";
                        fsType = "tmpfs";
                      };
                      system.stateVersion = "25.05";
                    }
                  ];
                };
                inherit (nixos.config.systemd) services;
                start = builtins.unsafeDiscardStringContext services.luks-ssh-unlock-smoke.serviceConfig.ExecStart;
                dhcp = builtins.unsafeDiscardStringContext services.luks-ssh-unlock-dhcp-listener-smoke.serviceConfig.ExecStart;
                console = builtins.fromJSON nixos.config.environment.etc."luks-ssh-unlock/smoke.console.json".text;
                vnc = builtins.fromJSON nixos.config.environment.etc."luks-ssh-unlock/smoke.vnc.json".text;
                inherit (nixos.config.systemd) sockets;
                basicHelper = services."luks-ssh-unlock-helper-basic@".serviceConfig;
                adminHelper = services."luks-ssh-unlock-helper-admin@".serviceConfig;
                users = nixos.config.users.users;
              in
              # The helper's sockets decide what a connection may do: each is
              # reachable by its own group only, never by everyone.
              assert sockets.luks-ssh-unlock-helper-basic.socketConfig.SocketMode == "0660";
              assert sockets.luks-ssh-unlock-helper-basic.socketConfig.SocketGroup == "luks-ssh-unlock";
              assert sockets.luks-ssh-unlock-helper-admin.socketConfig.SocketMode == "0660";
              assert sockets.luks-ssh-unlock-helper-admin.socketConfig.SocketGroup == "luks-ssh-unlock-admin";
              assert sockets.luks-ssh-unlock-helper-basic.socketConfig.Accept;
              assert nixpkgs.lib.hasInfix "helper serve --policy basic" (
                builtins.unsafeDiscardStringContext basicHelper.ExecStart
              );
              assert nixpkgs.lib.hasInfix "--policy admin" (
                builtins.unsafeDiscardStringContext adminHelper.ExecStart
              );
              assert basicHelper.CapabilityBoundingSet == [ "" ] && basicHelper.NoNewPrivileges;
              assert basicHelper.StandardInput == "socket";
              assert builtins.elem "luks-ssh-unlock" users.alice.extraGroups;
              assert !(builtins.elem "luks-ssh-unlock-admin" users.alice.extraGroups);
              assert builtins.elem "luks-ssh-unlock-admin" users.bob.extraGroups;
              assert nixos.config.environment.etc."luks-ssh-unlock/smoke.env".mode == "0600";
              assert vnc.type == "vnc";
              assert
                vnc.parameters == {
                  host = "127.0.0.1";
                  port = 5901;
                  passwordFile = "/run/secrets/vnc";
                  viewOnly = true;
                  resizeRemote = false;
                  allowRemote = false;
                };
              assert console.type == "oci";
              assert console.parameters.identity == "/home/user/.ssh/id_rsa";
              assert start == builtins.unsafeDiscardStringContext "${luks-ssh-unlock}/bin/luks-ssh-unlock";
              assert nixpkgs.lib.hasPrefix
                (builtins.unsafeDiscardStringContext "${luks-ssh-unlock}/bin/luks-ssh-unlock-dhcp-listener")
                dhcp;
              pkgs.runCommand "luks-ssh-unlock-nixos-module-check" { } "touch $out";
          };

          formatter = pkgs.nixfmt;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ luks-ssh-unlock ];
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              cargo-deny
              rust-analyzer
              shellcheck
              nixfmt
              statix
              deadnix
              openssh
            ];
          };
        }
      );
    in
    eachSystem
    // {
      # Linux package builds and checks run on the self-hosted nixbot.
      # aarch64-linux builds natively on GitHub's arm runners (the nix-aarch64
      # job of .github/workflows/build.yaml).
      hydraJobs = nixpkgs.lib.genAttrs [ "x86_64-linux" ] (system: {
        packages = self.packages.${system};
        checks = self.checks.${system};
      });

      overlays.default = _final: prev: {
        luks-ssh-unlock = packageFor prev;
      };

      # The flake's own package is the default, so the module needs no overlay.
      # The key makes NixOS import it once, however many modules import it.
      nixosModules.default = {
        key = "github:pschmitt/luks-ssh-unlock#nixosModules.default";
        imports = [
          ./nix/module.nix
          ./nix/helper.nix
          (
            { lib, pkgs, ... }:
            {
              services.luks-ssh-unlock.package =
                lib.mkDefault
                  self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            }
          )
        ];
      };
      nixosModules.luks-ssh-unlock = self.nixosModules.default;
    };
}
