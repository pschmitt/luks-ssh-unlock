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
- `packages.<system>.busybox-static`: the BusyBox bundle for paranoid checks.
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
- `initrdCheck.requireSignature` refuses unsigned baselines; enable it once
  every target has a signed one, and set `initrdCheck.dir` so baselines are
  fetched.

`nix/module.nix` documents every option.
