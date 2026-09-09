# Packaging

The `.deb` is the install contract. There is no tarball and no curl-to-shell
installer: this is a root daemon whose whole value is the systemd unit around
it, and a package manager is what installs, starts, upgrades and — crucially —
*stops* one of those correctly.

## Layout

```text
/usr/bin/macfand
/usr/lib/systemd/system/macfand.service
/usr/share/doc/macfand/macfand.toml.example
```

`/etc/macfand.toml` is deliberately outside the manifest. Every key is optional
and the built-in defaults are a complete configuration, so there is nothing that
has to be shipped there — and a package that owns a config is a package that
argues with the operator's edits on every upgrade.

## Maintainer scripts

| Path | What it does |
|---|---|
| `deb/postinst` | `unmask`, then `enable` on a fresh install; `start` on a fresh install, `try-restart` on an upgrade |
| `deb/prerm` | `stop` on removal, so `ExecStopPost` hands the fans back to the SMC before the binary goes |
| `deb/postrm` | `mask` on remove, `purge` + `unmask` on purge, then `daemon-reload` once the unit file is gone |

Every systemd action goes through `deb-systemd-helper` and `deb-systemd-invoke`,
the same helpers `dh_installsystemd` generates calls to, rather than through
`systemctl` directly — which is what makes the opt-outs below work at all, and
what makes the package remember across an upgrade or a reinstall that an
operator disabled the unit. Hence `init-system-helpers` in `Depends`.

Whether macfand is enabled is the operator's state, not the package's:
`prerm` stops the unit but does not disable it, and only a `purge` discards the
enable state and the symlinks behind it.

`postinst` refuses to enable or start macfand while an `mbpfan` or `macfanctld`
that dpkg does not know about is running — two daemons writing `fan1_output`
overwrite each other. The packaged ones are handled by `Conflicts` in the
control file instead.

## Installing without starting it

Installing enables and starts the daemon, because a fan daemon that is installed
but not running is worse than one that was never installed: the operator
believes the fan is being driven, and it is not. For the cases where that is not
wanted — image builds, chroots, a machine being staged, a config not written yet
— there are three admin-side opt-outs, all of them standard Debian mechanisms
rather than anything macfand invents.

**A preset file** is the one to reach for from Ansible or any other config
management, because it is declarative, idempotent, and survives upgrades and
reinstalls. Write it *before* installing:

```text
# /etc/systemd/system-preset/10-macfand.preset
disable macfand.service
```

`deb-systemd-helper enable` runs `systemctl preset --preset-mode=enable-only` on
a first install, so the unit is never enabled; `deb-systemd-invoke start` then
declines to start a disabled unit and says so on stderr. Both boot and install
are covered by the one file.

```yaml
- name: macfand must not enable itself on install
  copy:
    dest: /etc/systemd/system-preset/10-macfand.preset
    content: "disable macfand.service\n"
    mode: "0644"

- name: install macfand
  apt: { deb: /tmp/macfand-linux-amd64.deb }
```

**`policy-rc.d`** blocks the start for the duration of one install and nothing
else — the unit is still enabled, so it comes up at the next boot. This is the
chroot and container idiom:

```sh
printf '#!/bin/sh\nexit 101\n' >/usr/sbin/policy-rc.d && chmod +x /usr/sbin/policy-rc.d
apt install ./dist/macfand-linux-amd64.deb
rm /usr/sbin/policy-rc.d
```

**`systemctl mask macfand.service`** works on a unit that does not exist yet, and
`postinst`'s `unmask` lifts only the mask a previous `postrm` took — a mask taken
by hand has no state file behind it and is left alone. Use it to hold a machine
off macfand indefinitely.

What macfand deliberately does *not* have is a config key for this. A daemon
that `systemctl status` reports as active while it has decided not to drive the
fans is the exact confusion the unit exists to prevent, and a key in
`/etc/macfand.toml` cannot stop the start that `postinst` performs before that
file is necessarily written. Use `systemctl disable` — or one of the three
above — and let the service state be the single answer to "is macfand running
this machine's fans".

## Build

```sh
bash packaging/build-deb.sh   # writes dist/macfand-linux-amd64.deb
```

Needs `dpkg-deb`, `objdump` and a Rust toolchain. The `libc6` floor is read out
of the built binary's versioned GLIBC symbols rather than hardcoded, so it
tracks whatever the release builder happens to be. amd64 only — there is no
arm64 machine with an Apple SMC.
