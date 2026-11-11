{
  lib,
  stdenv,
  runCommand,
  # crane's library for this nixpkgs (`crane.mkLib pkgs`).
  craneLib,
  installShellFiles,
  makeWrapper,
  bash,
  coreutils,
  cpio,
  diffutils,
  findutils,
  gzip,
  msmtp,
  openssh,
  util-linux,
  zstd,
  busyboxBundle,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
  inherit (cargoToml.package) version;
  # The package's own version, normalized out of everything the dependency
  # build sees, so a version bump keeps that derivation (and its caches).
  normalizedContents =
    file:
    builtins.replaceStrings
      [ "name = \"luks-ssh-unlock\"\nversion = \"${version}\"" ]
      [ "name = \"luks-ssh-unlock\"\nversion = \"0.0.0\"" ]
      (builtins.readFile file);
  normalized = file: builtins.toFile (baseNameOf file) (normalizedContents file);
  common = {
    pname = "luks-ssh-unlock";
    inherit version src;
    strictDeps = true;
    # Vendored crates don't depend on the package's version: shared by both builds.
    cargoVendorDir = craneLib.vendorCargoDeps {
      cargoLockContents = normalizedContents ../Cargo.lock;
    };
    cargoExtraArgs = "--locked";
  };
  # Only the normalized manifests and a stub main, unlike crane's mkDummySrc,
  # which also copies the real ones.
  depsSrc = runCommand "luks-ssh-unlock-deps-src" { } ''
    mkdir -p $out/src
    cp ${normalized ../Cargo.toml} $out/Cargo.toml
    cp ${normalized ../Cargo.lock} $out/Cargo.lock
    echo 'fn main() {}' > $out/src/main.rs
  '';
  # The dependencies, built once: source changes and version bumps reuse them.
  deps = craneLib.buildDepsOnly (
    # dummySrc replaces src; passing both makes crane warn.
    builtins.removeAttrs common [ "src" ]
    // {
      pname = "luks-ssh-unlock-deps";
      version = "0";
      dummySrc = depsSrc;
    }
  );
  # Only what the binary is built from, so editing docs, Nix files or CI does
  # not rebuild it.
  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  # Tools the unlocker runs: OpenSSH for every connection, msmtp as sendmail.
  runtimePath = lib.makeBinPath [
    openssh
    msmtp
  ];
  checksumPath = lib.makeBinPath [
    bash
    coreutils
    cpio
    diffutils
    findutils
    gzip
    openssh
    util-linux
    zstd
  ];
in
craneLib.buildPackage (
  common
  // {
    cargoArtifacts = deps;

    nativeBuildInputs = [
      installShellFiles
      makeWrapper
    ];
    # The unit tests run here, reusing the built dependencies; they spawn sh,
    # cat and sleep.
    doCheck = true;
    nativeCheckInputs = [ coreutils ];

    postInstall = ''
      install -Dm755 ${../initrd-checksum.sh} $out/bin/initrd-checksum
      patchShebangs $out/bin/initrd-checksum
      wrapProgram $out/bin/initrd-checksum --prefix PATH : ${checksumPath}

      wrapProgram $out/bin/luks-ssh-unlock \
        --prefix PATH : ${runtimePath} \
        --set-default INITRD_CHECKSUM_SCRIPT $out/bin/initrd-checksum \
        --set-default INITRD_CHECKSUM_BUSYBOX_DIR $out/busybox

      # The NixOS module starts the DHCP listener under this name.
      makeWrapper $out/bin/luks-ssh-unlock $out/bin/luks-ssh-unlock-dhcp-listener \
        --add-flags dhcp-listener

      mkdir -p $out/busybox
      cp ${busyboxBundle}/busybox-amd64 ${busyboxBundle}/busybox-arm64 $out/busybox/
      ln -s busybox-amd64 $out/busybox/busybox
    ''
    + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
      for shell in bash zsh fish; do
        $out/bin/luks-ssh-unlock completions $shell > luks-ssh-unlock.$shell
      done
      installShellCompletion --cmd luks-ssh-unlock \
        --bash luks-ssh-unlock.bash \
        --zsh luks-ssh-unlock.zsh \
        --fish luks-ssh-unlock.fish
    '';

    meta = {
      description = "Unlock LUKS-encrypted hosts remotely over SSH";
      homepage = "https://github.com/pschmitt/luks-ssh-unlock";
      license = lib.licenses.gpl3Only;
      maintainers = [ lib.maintainers.pschmitt ];
      mainProgram = "luks-ssh-unlock";
      platforms = lib.platforms.linux;
    };
  }
)
