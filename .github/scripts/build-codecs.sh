#!/bin/sh
# Builds the static libvpx and libopus with build.rs in a manylinux or Alpine
# container. Run it in the repository root; the archives end up in
# target/debug/build/getstream-*/out/native/<target>.
set -eu

nasm=""
if [ "$(uname -m)" = x86_64 ]; then
  nasm=nasm
fi

. /etc/os-release
case "$ID" in
  almalinux) dnf install -y perl $nasm ;;
  alpine) apk add --no-cache curl diffutils gcc make musl-dev perl $nasm ;;
  *) echo "unsupported distribution: $ID" >&2; exit 1 ;;
esac

curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"

cargo check --locked --lib
