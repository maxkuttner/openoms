#!/bin/sh
# Render the Homebrew formula for one release.
#
#   sh packaging/homebrew/render.sh <version> <SHA256SUMS> > Formula/openoms.rb
#
# <version> has no leading v (0.6.0). Fails, printing nothing to stdout, if a
# platform's checksum is missing: a formula with an empty sha256 would publish.
set -eu

version="${1:-}"
sums="${2:-}"
here="$(cd "$(dirname "$0")" && pwd)"

die() { echo "render.sh: $*" >&2; exit 1; }

case "$version" in
	"") die "usage: render.sh <version> <SHA256SUMS>" ;;
	v*) die "version $version must not start with v" ;;
esac
[ -r "$sums" ] || die "cannot read $sums"

# The hash for one tarball, matched by exact file name (sha256sum's "  " or " *").
sha_for() {
	awk -v f="$1" '($2 == f || $2 == "*" f) && length($1) == 64 { print $1; exit }' "$sums"
}

darwin="$(sha_for oms-aarch64-apple-darwin.tar.gz)"
linux="$(sha_for oms-x86_64-unknown-linux-gnu.tar.gz)"
[ -n "$darwin" ] || die "no sha256 for oms-aarch64-apple-darwin.tar.gz in $sums"
[ -n "$linux" ] || die "no sha256 for oms-x86_64-unknown-linux-gnu.tar.gz in $sums"

sed -e "s/{{version}}/$version/g" \
	-e "s/{{sha256_darwin_arm64}}/$darwin/g" \
	-e "s/{{sha256_linux_x86_64}}/$linux/g" \
	"$here/openoms.rb.tmpl"
