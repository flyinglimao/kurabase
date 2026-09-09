#!/bin/sh
set -eu
KURA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export CARGO_HOME="$KURA_ROOT/.tools/cargo"
export RUSTUP_HOME="$KURA_ROOT/.tools/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
exec cargo "$@"
