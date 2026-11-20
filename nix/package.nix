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
  gnugrep,
  msmtp,
  openssh,
  zstd,
  # The static initrd measurers (nix/measure.nix).
  initrdMeasure,
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
    mkdir -p $out/src $out/initrd-measure/src
    cp ${normalized ../Cargo.toml} $out/Cargo.toml
    cp ${normalized ../Cargo.lock} $out/Cargo.lock
    cp ${../initrd-measure/Cargo.toml} $out/initrd-measure/Cargo.toml
    echo 'fn main() {}' > $out/src/main.rs
    echo 'fn main() {}' > $out/initrd-measure/src/main.rs
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
      # The workspace's other member; its static builds are nix/measure.nix.
      ../initrd-measure/Cargo.toml
      ../initrd-measure/src
    ];
  };
  # Tools the unlocker runs: OpenSSH for every connection, msmtp as sendmail,
  # and coreutils for msmtp's `passwordeval` commands (usually `cat FILE`).
  runtimePath = lib.makeBinPath [
    coreutils
    openssh
    msmtp
  ];
  # initrd-checksum only orchestrates: initrd-measure does the measuring.
  checksumPath = lib.makeBinPath [
    bash
    coreutils
    diffutils
    openssh
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
      # Not patchShebangs: with strictDeps it silently leaves the
      # `#!/usr/bin/env bash` shebang in place (no bash on its search path),
      # and /usr/bin/env does not exist in every environment the module's
      # activation script runs in (nixos-enter, for example). --replace-fail
      # breaks the build if the shebang ever changes.
      substitute ${../initrd-checksum.sh} $out/bin/initrd-checksum \
        --replace-fail '#!/usr/bin/env bash' '#!${bash}/bin/bash'
      chmod 755 $out/bin/initrd-checksum
      mkdir -p $out/libexec/initrd-measure
      cp ${initrdMeasure}/initrd-measure-* $out/libexec/initrd-measure/
      wrapProgram $out/bin/initrd-checksum \
        --prefix PATH : ${checksumPath} \
        --set-default INITRD_CHECKSUM_MEASURE_DIR $out/libexec/initrd-measure

      wrapProgram $out/bin/luks-ssh-unlock \
        --prefix PATH : ${runtimePath} \
        --set-default INITRD_CHECKSUM_SCRIPT $out/bin/initrd-checksum

      # The NixOS module starts the DHCP listener under this name.
      makeWrapper $out/bin/luks-ssh-unlock $out/bin/luks-ssh-unlock-dhcp-listener \
        --add-flags dhcp-listener
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

    # initrd-checksum runs where the caller's PATH may hold next to nothing
    # (the module's activation script, nixos-enter). Run it with an empty
    # environment on an initrd shaped like NixOS's (an uncompressed early
    # archive, padding, then a zstd archive) made by the real cpio and zstd,
    # so a tool missing from checksumPath fails the build instead of a host's
    # activation, and the measurer reads what those tools write.
    doInstallCheck = true;
    nativeInstallCheckInputs = [
      coreutils
      cpio
      gnugrep
      zstd
    ];
    installCheckPhase = ''
      runHook preInstallCheck

      mkdir -p fixture/early/kernel fixture/main/etc
      echo microcode > fixture/early/kernel/ucode.bin
      echo main > fixture/main/etc/initrd-release
      (cd fixture/early && find . | cpio --quiet -o -H newc) > fixture/initrd
      head -c 512 /dev/zero >> fixture/initrd
      (cd fixture/main && find . | cpio --quiet -o -H newc | zstd -q -c) >> fixture/initrd

      env -i TMPDIR="$TMPDIR" $out/bin/initrd-checksum check \
        --initrd="$PWD/fixture/initrd" > fixture/checksum
      grep -q ' /kernel/ucode.bin$' fixture/checksum
      grep -q ' /etc/initrd-release$' fixture/checksum
      grep -qx '# initrd-measure format 2' fixture/checksum

      # The image and the tree it unpacks to measure the same, in both
      # formats: the comparison a check of a running initrd makes.
      mkdir fixture/tree
      (cd fixture/early && find . | cpio --quiet -pdm ../tree)
      (cd fixture/main && find . | cpio --quiet -pdm ../tree)
      for format in 1 2
      do
        $out/libexec/initrd-measure/initrd-measure-$(uname -m) --format $format \
          image fixture/initrd > fixture/image.$format
        $out/libexec/initrd-measure/initrd-measure-$(uname -m) --format $format \
          live-root --root fixture/tree > fixture/tree.$format
        cmp fixture/image.$format fixture/tree.$format
      done

      # A tail that is neither compressed nor cpio must fail with a short
      # error, not feed garbage to cpio.
      (cd fixture/early && find . | cpio --quiet -o -H newc) > fixture/bad
      head -c 65536 /dev/urandom | tr -d '\037\050' >> fixture/bad
      if env -i TMPDIR="$TMPDIR" $out/bin/initrd-checksum check \
        --initrd="$PWD/fixture/bad" > /dev/null 2> fixture/bad.err
      then
        echo "initrd-checksum accepted a garbage initrd tail" >&2
        exit 1
      fi
      if [ "$(wc -l < fixture/bad.err)" -gt 20 ]
      then
        echo "initrd-checksum flooded stderr on a garbage initrd tail:" >&2
        head -n 20 fixture/bad.err >&2
        exit 1
      fi

      runHook postInstallCheck
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
