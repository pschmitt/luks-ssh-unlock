{
  description = "Auto-unlock remote hosts via SSH and Kubernetes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        luks-ssh-unlock = pkgs.callPackage ./package.nix { };
      in
      {
        packages = {
          default = luks-ssh-unlock;
          luks-ssh-unlock = luks-ssh-unlock;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
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
    ) // {
      # NixOS module
      nixosModules = {
        default = ./module.nix;
        luks-ssh-unlock = ./module.nix;
      };
    };
}