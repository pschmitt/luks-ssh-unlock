{
  description = "Auto-unlock remote hosts via SSH and Kubernetes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        busybox-static = pkgs.callPackage ./nix/busybox.nix {
          busyboxAmd64 = nixpkgs.legacyPackages.x86_64-linux.pkgsStatic.busybox;
          busyboxArm64 = nixpkgs.legacyPackages.aarch64-linux.pkgsStatic.busybox;
        };
        luks-ssh-unlock = pkgs.callPackage ./nix/package.nix {
          busyboxBundle = busybox-static;
        };
        luks-ssh-unlock-entrypoint = pkgs.callPackage ./nix/entrypoint.nix { };
      in
      {
        packages = {
          inherit luks-ssh-unlock;
          inherit busybox-static;
          default = luks-ssh-unlock;
          entrypoint = luks-ssh-unlock-entrypoint;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            coreutils
            findutils
            util-linux
            cpio
            gzip
            zstd
            curl
            dig
            jq
            gnugrep
            msmtp
            netcat-gnu
            openssh
          ];
        };
      }
    )
    // {
      # NixOS module
      nixosModules = {
        default = ./nix/module.nix;
        luks-ssh-unlock = ./nix/module.nix;
      };
    };
}
