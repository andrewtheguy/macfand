#!/usr/bin/env bash
# Build dist/macfand-linux-amd64.deb from this checkout.
#
# The package owns the FHS paths a distro package is expected to own:
#
#   /usr/bin/macfand
#   /usr/lib/systemd/system/macfand.service
#   /usr/share/doc/macfand/macfand.toml.example
#
# The live config at /etc/macfand.toml is deliberately not package-owned. Every
# key is optional and the built-in defaults are a complete, working
# configuration, so there is nothing that has to be shipped there — and a
# package that owns a config is a package that argues with the operator's edits
# on every upgrade. The annotated example goes to /usr/share/doc for copying.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Intel Macs only: there is no arm64 machine with an Apple SMC for this to run
# on, so there is nothing to cross-build for either.
[ "$(uname -m)" = x86_64 ] || {
    echo "macfand only runs on Intel Macs, and $(uname -m) is not one" >&2
    exit 1
}
command -v dpkg-deb >/dev/null 2>&1 || { echo "dpkg-deb is required" >&2; exit 1; }
command -v objdump >/dev/null 2>&1 || { echo "objdump is required" >&2; exit 1; }

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$' <<<"$version" || {
    echo "invalid version in Cargo.toml: '$version'" >&2
    exit 1
}

cargo build --release
binary=target/release/macfand

reported="$("$binary" --version)"
[ "$reported" = "macfand $version" ] || {
    echo "built binary reports '$reported', expected 'macfand $version'" >&2
    exit 1
}

# Pin the libc floor to what this binary actually references rather than to a
# guess. Too low and the package installs onto a system where it cannot exec;
# too high and it refuses to install onto one where it would have run.
glibc_min="$(objdump -T "$binary" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' \
    | sort -V | tail -1)"
[ -n "$glibc_min" ] || { echo "could not read the binary's glibc requirement" >&2; exit 1; }

mkdir -p dist tmp
stage="$(mktemp -d "$repo_root/tmp/deb.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

root="$stage/root"
install -D -m 755 "$binary" "$root/usr/bin/macfand"
install -D -m 644 systemd/macfand.service "$root/usr/lib/systemd/system/macfand.service"
install -D -m 644 macfand.toml.example "$root/usr/share/doc/macfand/macfand.toml.example"

# '-' separates the Debian revision, so a SemVer prerelease has to become '~',
# which sorts before everything: '0.0.1-rc.1-1' would otherwise sort *after*
# the '0.0.1-1' release. '+' is left alone — it is legal in a Debian version and
# already sorts after the plain release, which is what build metadata means.
deb_version="${version//-/~}"

mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: macfand
Version: ${deb_version}-1
Architecture: amd64
Maintainer: andrewtheguy <andrewchen5678@gmail.com>
Section: admin
Priority: optional
Depends: libc6 (>= ${glibc_min}), systemd
Conflicts: mbpfan, macfanctld
Homepage: https://github.com/andrewtheguy/macfand
Description: Multi-sensor fan control daemon for Intel MacBooks
 Runs a PI controller per configured sensor and drives the Apple SMC fans at
 whichever one is asking for the most air, so a hot chassis or PCH raises the
 fan even when the CPU package is comfortable.
 .
 Installing enables and starts macfand.service. It Conflicts with mbpfan and
 macfanctld because two daemons writing fan1_output overwrite each other.
CONTROL

install -m 755 packaging/deb/postinst "$root/DEBIAN/postinst"
install -m 755 packaging/deb/prerm "$root/DEBIAN/prerm"
install -m 755 packaging/deb/postrm "$root/DEBIAN/postrm"

output="dist/macfand-linux-amd64.deb"
dpkg-deb --build --root-owner-group "$root" "$output"

[ "$(dpkg-deb --field "$output" Package)" = macfand ]
dpkg-deb --contents "$output" > "$stage/contents"
grep -q ' \./usr/bin/macfand$' "$stage/contents"
grep -q ' \./usr/lib/systemd/system/macfand.service$' "$stage/contents"
grep -q ' \./usr/share/doc/macfand/macfand.toml.example$' "$stage/contents"

echo ">> wrote $output (macfand $version, libc6 >= $glibc_min)"
