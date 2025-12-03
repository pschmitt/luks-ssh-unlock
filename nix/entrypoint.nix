{
  stdenv,
  lib,
  makeWrapper,
  coreutils,
  bash,
}:

stdenv.mkDerivation {
  pname = "luks-ssh-unlock-entrypoint";
  version = "0.1.0";

  src = ../entrypoint.sh;

  nativeBuildInputs = [ makeWrapper ];

  dontUnpack = true;

  installPhase = ''
    mkdir -p $out/bin
    cp $src $out/bin/entrypoint
    chmod +x $out/bin/entrypoint
    patchShebangs $out/bin

    wrapProgram $out/bin/entrypoint --set PATH "/app/bin:${coreutils}/bin:${bash}/bin:$PATH"
  '';

  meta = with lib; {
    description = "Entrypoint for luks-ssh-unlock container";
    platforms = platforms.all;
  };
}
