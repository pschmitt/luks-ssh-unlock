# A real initrd check: the target stops in its systemd initrd with SSH up,
# and the controller measures it over SSH with initrd-checksum against the
# baseline its image gives. Then it tampers with the running initrd, and
# finally boots the target and compares the baseline its activation wrote.
{
  pkgs,
  nixosModule,
  package,
}:
let
  # The snakeoil keys of nixpkgs' own initrd SSH test.
  keys = "${pkgs.path}/nixos/tests/initrd-network-ssh";
  hostKey = "/etc/ssh/initrd/ssh_host_ed25519_key";
in
pkgs.testers.runNixOSTest {
  name = "luks-ssh-unlock-initrd-checksum";

  nodes = {
    target =
      { lib, ... }:
      {
        imports = [ nixosModule ];
        services.luks-ssh-unlock = {
          # Without instances: only the activation snippet.
          enable = true;
          inherit package;
          activationScript.enable = true;
          activationScript.push = {
            enable = true;
            host = "controller";
            identityFile = "/etc/push-key";
            knownHostsFile = "/etc/controller-known-hosts";
            destination = "/tmp/baselines/target/initrd-checksum";
            instance = "target";
            timeout = 20;
          };
        };
        environment.etc.push-key = {
          source = "${keys}/id_ed25519";
          mode = "0600";
        };
        environment.etc.controller-known-hosts.text = "controller ${lib.readFile "${keys}/ssh_host_ed25519_key.pub"}";
        services.openssh.enable = true;
        testing.initrdBackdoor = true;
        boot.initrd.network = {
          enable = true;
          ssh = {
            enable = true;
            authorizedKeys = [ (lib.readFile "${keys}/id_ed25519.pub") ];
            # Where real deployments keep it, so it is left out like there.
            hostKeys = [ hostKey ];
          };
        };
        boot.initrd.secrets.${hostKey} = lib.mkForce "${keys}/ssh_host_ed25519_key";
      };

    controller = { lib, ... }: {
      environment = {
        systemPackages = [ package ];
        etc."ssh/test_host_key" = {
          source = "${keys}/ssh_host_ed25519_key";
          mode = "0600";
        };
        etc.sshKey = {
          source = "${keys}/id_ed25519";
          mode = "0600";
        };
      };
      services.openssh = {
        enable = true;
        settings.PermitRootLogin = "yes";
        hostKeys = [
          {
            path = "/etc/ssh/test_host_key";
            type = "ed25519";
          }
        ];
      };
      users.users.root.openssh.authorizedKeys.keys = [ (lib.readFile "${keys}/id_ed25519.pub") ];
    };
  };

  testScript =
    { nodes, ... }:
    let
      image = "${nodes.target.system.build.initialRamdisk}/${nodes.target.system.boot.loader.initrdFile}";
    in
    ''
      check = "initrd-checksum check --quiet --insecure-ssh -i /etc/sshKey -H target"

      start_all()
      controller.wait_for_unit("network.target")
      controller.wait_for_unit("sshd.service")
      with controller.nested("waiting for the initrd's SSH server"):
          retry(lambda _: controller.execute("nc -z target 22")[0] == 0)

      with subtest("the image gives the baseline, in both formats"):
          controller.succeed("initrd-checksum check --initrd=${image} > /tmp/v2")
          controller.succeed("initrd-checksum check --format 1 --initrd=${image} > /tmp/v1")
          assert controller.succeed("head -n1 /tmp/v2").strip() == "# initrd-measure format 2"
          print(controller.succeed("wc -l /tmp/v1 /tmp/v2; grep -c '^symlink:' /tmp/v2"))

      with subtest("the running initrd matches its image"):
          for fmt in ["v1", "v2"]:
              status, out = controller.execute(f"{check} --diff /tmp/{fmt} 2>&1")
              assert status == 0, f"{fmt} differs from the running initrd:\n{out}"

      with subtest("a new file is found in both formats"):
          target.succeed("echo evil > /etc/evil")
          for fmt in ["v1", "v2"]:
              out = controller.fail(f"{check} --diff /tmp/{fmt} 2>&1")
              assert "+" in out and "/etc/evil" in out, out
          target.succeed("rm /etc/evil")

      with subtest("a redirected symlink is found in format 2 only"):
          line = controller.succeed("grep -m1 '^symlink:/nix/store/.*  /etc/' /tmp/v2").strip()
          value, path = line.split("  ", 1)
          old = value.removeprefix("symlink:")
          target.succeed(f"ln -sfn /run/evil {path}")
          out = controller.fail(f"{check} --diff /tmp/v2 2>&1")
          assert f"symlink:/run/evil  {path}" in out, out
          controller.succeed(f"{check} --diff /tmp/v1")
          target.succeed(f"ln -sfn {old} {path}")
          controller.succeed(f"{check} --diff /tmp/v2")

      with subtest("the activation baseline is the image's"):
          target.switch_root()
          target.wait_for_unit("multi-user.target")
          booted = target.succeed("sha256sum < /etc/initrd-checksum/checksum")
          expected = controller.succeed("sha256sum < /tmp/v2")
          assert booted == expected, "activation baseline differs from the image's"

      with subtest("the baseline is signed with the host key"):
          # A first boot activates before sshd generated its host key; any
          # later activation signs.
          target.wait_for_unit("sshd.service")
          target.succeed("/run/current-system/activate")
          target.succeed(
              "echo \"target $(cat /etc/ssh/ssh_host_ed25519_key.pub)\" > /tmp/signers",
              "ssh-keygen -Y verify -f /tmp/signers -I target -n initrd-checksum"
              + " -s /etc/initrd-checksum/checksum.sig < /etc/initrd-checksum/checksum",
          )
          controller.succeed("cmp /tmp/baselines/target/initrd-checksum/checksum /tmp/v2")
          controller.succeed("test -s /tmp/baselines/target/initrd-checksum/checksum.sig")
          controller.succeed("journalctl --no-pager _SYSTEMD_UNIT=luks-ssh-unlock-target.service + SYSLOG_IDENTIFIER=luks-ssh-unlock-push-target | grep 'Received signed initrd baseline'")

      with subtest("a busy controller cannot hold up activation"):
          controller.succeed("systemd-run --unit=hold-baseline-lock ${pkgs.util-linux}/bin/flock /tmp/baselines/target ${pkgs.coreutils}/bin/sleep 60")
          retry(lambda _: controller.execute("flock -n /tmp/baselines/target true")[0] != 0)
          target.succeed("timeout 120 /run/current-system/activate")
          controller.succeed("systemctl stop hold-baseline-lock.service")

      with subtest("an unreachable controller does not fail activation"):
          controller.succeed("systemctl stop sshd.service")
          target.succeed("timeout 120 /run/current-system/activate")
    '';
}
