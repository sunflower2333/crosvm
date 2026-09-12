#!/bin/bash

# Run cargo_embargo with the appropriate arguments.

set -e -u

function usage() { echo "$0 [-r]" && exit 1; }
CROSVM_DIR="$ANDROID_BUILD_TOP/external/crosvm"
REUSE=""
while getopts 'r' FLAG; do
  case ${FLAG} in
    r)
      REUSE="--reuse-cargo-out --cargo-out-dir $CROSVM_DIR"
      ;;
    ?)
      echo "unknown flag."
      usage
      ;;
  esac
done

if ! [ -x "$(command -v bpfmt)" ]; then
  echo 'Error: bpfmt not found.' >&2
  exit 1
fi

# If there is need to verify installation of some packages, add them here in pkges.
pkges='
libdrm-dev
libcap-dev
libepoxy-dev
libwayland-dev
meson
pkg-config
protobuf-compiler
wayland-protocols
'
for pkg in $pkges; do
  set +e; result="$(dpkg-query -W --showformat='${db:Status-Status}' "$pkg" 2>&1)"; set -e
  if [ ! $? = 0 ] || [ ! "$result" = installed ]; then
    echo $pkg' not found. Please install.' >&2
    exit 1
  fi
done

# Use the specific rust version that crosvm upstream expects.
#
# TODO: Consider reading the toolchain from external/crosvm/rust-toolchain
#
# TODO: Consider using android's prebuilt rust binaries. Currently doesn't work
# because they try to incorrectly use system clang and llvm.
RUST_TOOLCHAIN="1.77.2"
rustup which --toolchain $RUST_TOOLCHAIN cargo || \
  rustup toolchain install $RUST_TOOLCHAIN
CARGO_BIN="$(dirname $(rustup which --toolchain $RUST_TOOLCHAIN cargo))"

cd "$CROSVM_DIR"

if [ ! "$REUSE" ]; then
  rm -f cargo.out cargo.metadata
  rm -rf target.tmp || /bin/true
fi

set -x
cargo_embargo $REUSE --cargo-bin "$CARGO_BIN" generate cargo_embargo.json
set +x

if [ ! "$REUSE" ]; then
  rm -f cargo.out cargo.metadata
  rm -rf target.tmp || /bin/true
fi

# Revert changes to Cargo.lock caused by cargo_embargo.
#
# Android diffs in Cargo.toml files can cause diffs in the Cargo.lock when
# cargo_embargo runs. This didn't happen with cargo2android.py because it
# ignored the lock file.
git restore Cargo.lock
