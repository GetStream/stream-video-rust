#!/bin/sh
# Runs the unit tests of an extracted package in a Linux container that has
# only Rust and a C compiler, and checks that the test binary links no codec
# library at run time. Run it in the package directory.
set -eu

. /etc/os-release
case "$ID" in
  debian | ubuntu)
    apt-get update
    apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev
    ;;
  almalinux | fedora) dnf install -y gcc ;;
  alpine) apk add --no-cache curl gcc musl-dev ;;
  *) echo "unsupported distribution: $ID" >&2; exit 1 ;;
esac

curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"

cargo test --locked --lib

bin=$(find target/debug/deps -maxdepth 1 -type f -perm -100 -name "getstream-*")
test -n "$bin"
# A static musl binary makes ldd fail, which is also a pass.
! ldd $bin | grep -E "libvpx|libopus"
