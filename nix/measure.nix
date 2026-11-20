# The static initrd measurer, one binary per target arch: initrd-checksum
# uploads the one matching the target's `uname -m`. The build host's own arch
# comes from pkgsStatic, the other one is cross-compiled; both toolchains
# are in the binary cache.
{
  lib,
  pkgs,
  runCommand,
  binutils-unwrapped,
  crane,
}:
let
  cargoToml = lib.importTOML ../initrd-measure/Cargo.toml;
  # The workspace with only the measurer's sources; the root package gets a
  # stub main, so editing the unlocker does not rebuild the measurer.
  src = runCommand "initrd-measure-src" { } ''
    mkdir -p $out/src
    cp ${../Cargo.toml} $out/Cargo.toml
    cp ${../Cargo.lock} $out/Cargo.lock
    cp -r ${
      lib.fileset.toSource {
        root = ../initrd-measure;
        fileset = lib.fileset.unions [
          ../initrd-measure/Cargo.toml
          ../initrd-measure/src
        ];
      }
    } $out/initrd-measure
    echo 'fn main() {}' > $out/src/main.rs
  '';
  build =
    targetPkgs:
    (crane.mkLib targetPkgs).buildPackage {
      pname = "initrd-measure";
      inherit (cargoToml.package) version;
      inherit src;
      strictDeps = true;
      # Its few dependencies build with it.
      cargoArtifacts = null;
      cargoExtraArgs = "--locked -p initrd-measure";
      CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
      # The unit tests run where the binary can.
      doCheck = targetPkgs.stdenv.buildPlatform.canExecute targetPkgs.stdenv.hostPlatform;
    };
  inherit (pkgs.stdenv) hostPlatform;
  x86_64 = build (if hostPlatform.isx86_64 then pkgs.pkgsStatic else pkgs.pkgsCross.musl64);
  aarch64 = build (
    if hostPlatform.isAarch64 then pkgs.pkgsStatic else pkgs.pkgsCross.aarch64-multiplatform-musl
  );
in
runCommand "initrd-measure-${cargoToml.package.version}"
  {
    passthru = { inherit x86_64 aarch64; };
    meta = {
      description = "Static initrd measurer for luks-ssh-unlock (x86_64 + aarch64)";
      license = lib.licenses.gpl3Only;
      platforms = lib.platforms.linux;
    };
  }
  ''
    mkdir -p $out
    install -m 755 ${x86_64}/bin/initrd-measure $out/initrd-measure-x86_64
    install -m 755 ${aarch64}/bin/initrd-measure $out/initrd-measure-aarch64
    # It runs on targets with nothing but a kernel: no loader, no libc.
    for bin in $out/initrd-measure-*
    do
      if ${binutils-unwrapped}/bin/readelf -l "$bin" | grep -q INTERP
      then
        echo "$bin is dynamically linked" >&2
        exit 1
      fi
    done
  ''
