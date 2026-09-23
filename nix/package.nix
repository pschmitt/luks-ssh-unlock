{
  lib,
  stdenv,
  makeWrapper,
  bash,
  coreutils,
  curl,
  cpio,
  diffutils,
  dig,
  jq,
  gnugrep,
  msmtp,
  netcat-gnu,
  openssh,
  findutils,
  util-linux,
  gzip,
  zstd,
  python3,
  busyboxBundle,
}:

stdenv.mkDerivation {
  pname = "luks-ssh-unlock";
  version = "0.1.0";

  src = ../.;

  nativeBuildInputs = [ makeWrapper ];

  installPhase = ''
    mkdir -p $out/bin
    cp "$src/luks-ssh-unlock.sh" $out/bin/luks-ssh-unlock
    cp "$src/initrd-checksum.sh" $out/bin/initrd-checksum
    cp "$src/dhcp-listener.py" $out/bin/luks-ssh-unlock-dhcp-listener
    chmod +x $out/bin/luks-ssh-unlock
    chmod +x $out/bin/initrd-checksum
    chmod +x $out/bin/luks-ssh-unlock-dhcp-listener

    patchShebangs $out/bin

    wrapProgram $out/bin/luks-ssh-unlock --prefix PATH : ${
      lib.makeBinPath [
        bash
        coreutils
        dig
        curl
        gnugrep
        jq
        msmtp # for sendmail TODO: allow overriding this via build var
        netcat-gnu
        openssh
        python3
      ]
    }

    wrapProgram $out/bin/luks-ssh-unlock-dhcp-listener --prefix PATH : ${lib.makeBinPath [ python3 ]}

    wrapProgram $out/bin/initrd-checksum --prefix PATH : ${
      lib.makeBinPath [
        bash
        coreutils
        diffutils
        findutils
        util-linux
        cpio
        gzip
        zstd
        openssh
      ]
    }

    mkdir -p $out/busybox
    cp ${busyboxBundle}/busybox-amd64 $out/busybox/busybox-amd64
    cp ${busyboxBundle}/busybox-arm64 $out/busybox/busybox-arm64
    ln -s busybox-amd64 $out/busybox/busybox
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
