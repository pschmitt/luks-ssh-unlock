# Nix

## Flake outputs

```sh
nix build github:pschmitt/luks-ssh-unlock   # binary, initrd-checksum, DHCP listener, completions
nix run github:pschmitt/luks-ssh-unlock -- status --host target.example.com
nix develop                                 # cargo, clippy, rustfmt, cargo-deny, nixfmt, ...
nix flake check                             # package build with unit tests, module check
```

- `packages.<system>.luks-ssh-unlock` (also `default`), for x86_64-linux and
  aarch64-linux.
- `packages.<system>.initrd-measure`: the static measurers that
  `initrd-checksum` uploads (`initrd-measure-x86_64`, `initrd-measure-aarch64`).
- `checks.<system>.initrd-checksum-vm`: a VM in its initrd, checked over SSH.
- `overlays.default` adds `pkgs.luks-ssh-unlock`.
- `nixosModules.default`: the module, using this flake's package.

The package is built with [crane](https://github.com/ipetkov/crane): its
dependencies are a derivation of their own, so source changes and version
bumps reuse them from the cache.

## NixOS module

```nix
{
  services.luks-ssh-unlock = {
    enable = true;
    instances.target = {
      hostname = "target.example.com";
      key = "/run/secrets/luks-unlock-key";
      passphraseFile = "/run/secrets/luks-target";
      type = "systemd";
      initrdKnownHosts = "target.example.com ssh-ed25519 AAAA...";
      healthcheck = {
        enable = true;
        command = "systemctl is-system-running";
      };
    };
  };
}
```

- Each instance gets an environment file `/etc/luks-ssh-unlock/<name>.env` and
  a service `luks-ssh-unlock-<name>`. `passphraseFile` is passed as a systemd
  credential.
- Each instance also gets a `luks-ssh-unlock-<name>` command, which runs sudo
  itself when needed:

  | Command | Action |
  | --- | --- |
  | `luks-ssh-unlock-<name>` | One validated unlock attempt (`--force`/`-f` skips the integrity and host-key checks) |
  | `luks-ssh-unlock-<name> status` | Target state, the service state, the last unlock and recent logs |
  | `luks-ssh-unlock-<name> tui` | The dashboard for this target |
  | `luks-ssh-unlock-<name> logs` | Follow the service's journal |
  | `luks-ssh-unlock-<name> start`, `stop` | Control the service |

- `dhcpListener.enable` watches DHCP on an interface and starts a one-shot
  unlock as soon as the target's initrd gets a lease and answers on SSH. The
  unit gets `CAP_NET_RAW`.
- `activationScript.enable` computes the host's own initrd baseline on
  activation and signs it with `/etc/ssh/ssh_host_ed25519_key`.
- `activationScript.push` can send that signed baseline to the controller
  immediately, without waiting for its next polling tick:

  On the controller, enable the receiver for the target and supply the
  client's public key:

  ```nix
  services.luks-ssh-unlock.instances.target = {
    hostname = "target.example.com";
    initrdCheck.enable = true;
    initrdCheck.pushReceiver = {
      enable = true;
      authorizedKeys = [ "ssh-ed25519 AAAA..." ];
    };
  };
  ```

  The module creates this dedicated system account, its baseline directory,
  and an SSH forced command. Each target needs its own account and keys; the
  account cannot open a shell, allocate a TTY, or forward connections. It can
  submit only a bounded archive for that target's configured baseline path.
  The default username is `luks-push-` followed by the target's hostname
  without its domain. The client uses its own hostname by default. Set
  `user` on both sides if those hostnames differ. Use `authorizedKeysFile`
  instead of `authorizedKeys` for a runtime public-key file.

  ```nix
  services.luks-ssh-unlock.activationScript = {
    enable = true;
    push = {
      enable = true;
      host = "controller.example.com";
      identityFile = "/run/secrets/baseline-push-key";
      knownHostsFile = "/etc/ssh/controller-known-hosts";
      timeout = 15;
    };
  };
  ```

  With the module receiver, its configured target determines the destination
  and instance; the client-side values should match for compatibility with
  other controllers. Successful pushes are logged on the controller and appear
  in the corresponding instance's dashboard activity. Pin its host key in
  `knownHostsFile` and provision the private key outside the Nix store. A missing
  signing key skips the push. A failed
  or timed-out push only logs a warning; it does not fail activation. Polling
  remains enabled as a fallback. Check the warning and ensure the controller
  has the new baseline before rebooting when the push fails.
  Update the controller first: current versions serialize polling and pushes
  so a fetch already in progress cannot overwrite a newer pushed baseline.
- `initrdCheck.requireSignature` refuses unsigned baselines; enable it once
  every target has a signed one, and set `initrdCheck.dir` so baselines are
  fetched.

`nix/module.nix` documents every option.
