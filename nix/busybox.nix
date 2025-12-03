{
  stdenv,
  lib,
  busyboxAmd64,
  busyboxArm64,
}:

stdenv.mkDerivation {
  pname = "luks-ssh-unlock-busybox-bundle";
  version = "0.1.0";

  src = null;
  dontUnpack = true;

  installPhase = ''
    mkdir -p $out
    cp ${busyboxAmd64}/bin/busybox $out/busybox-amd64
    cp ${busyboxArm64}/bin/busybox $out/busybox-arm64
    ln -s busybox-amd64 $out/busybox
    chmod +x $out/busybox*
  '';

  meta = with lib; {
    description = "Static busybox bundle for luks-ssh-unlock (amd64 + arm64)";
    platforms = platforms.all;
  };
}
