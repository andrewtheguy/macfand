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
| `deb/postinst` | `daemon-reload`, then on a fresh install `enable --now`; on an upgrade `try-restart` only |
| `deb/prerm` | `disable` and `stop` on removal, so `ExecStopPost` hands the fans back to the SMC before the binary goes |
| `deb/postrm` | `daemon-reload` once the unit file is gone |

`postinst` refuses to start macfand while an `mbpfan` or `macfanctld` that dpkg
does not know about is running — two daemons writing `fan1_output` overwrite
each other. The packaged ones are handled by `Conflicts` in the control file
instead.

## Build

```sh
bash packaging/build-deb.sh   # writes dist/macfand-linux-amd64.deb
```

Needs `dpkg-deb`, `objdump` and a Rust toolchain. The `libc6` floor is read out
of the built binary's versioned GLIBC symbols rather than hardcoded, so it
tracks whatever the release builder happens to be. amd64 only — there is no
arm64 machine with an Apple SMC.
