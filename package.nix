{
  lib,
  stdenv,
  makeWrapper,
  curl,
  dig,
  jq,
  gnugrep,
  msmtp,
  netcat-gnu,
  openssh,
}:

stdenv.mkDerivation rec {
  pname = "luks-ssh-unlock";
  version = "0.1.0";

  src = ./.;

  nativeBuildInputs = [ makeWrapper ];

  installPhase = ''
    mkdir -p $out/bin
    cp luks-ssh-unlock.sh $out/bin/luks-ssh-unlock
    chmod +x $out/bin/luks-ssh-unlock

    wrapProgram $out/bin/luks-ssh-unlock --prefix PATH : ${
      lib.makeBinPath [
        dig
        curl
        gnugrep
        jq
        msmtp # for sendmail TODO: allow overriding this via build var
        netcat-gnu
        openssh
      ]
    }
  '';

  meta = with lib; {
    description = "Auto-unlock remote hosts via SSH and Kubernetes";
    homepage = "https://github.com/pschmitt/luks-ssh-unlock";
    license = licenses.gpl3Only;
    maintainers = with maintainers; [ pschmitt ];
    mainProgram = "luks-ssh-unlock";
    platforms = platforms.all;
  };
}