#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$root/include"
cbindgen --quiet --config "$root/cbindgen.toml" --crate anytls --output "$root/include/anytls.h" "$root"
