#!/usr/bin/env bash
set -euo pipefail

cargo build --release --locked --bin overgit
binary=target/release/overgit
version=$("$binary" --version)
version=${version#overgit }
target=$(rustc -vV | sed -n 's/^host: //p')
name="overgit-v${version}-${target}"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" target/dist
cp "$binary" README.md LICENSE "$stage/$name/"
tar -C "$stage" -czf "target/dist/$name.tar.gz" "$name"
mkdir "$stage/extracted"
tar -C "$stage/extracted" -xzf "target/dist/$name.tar.gz"
test "$("$stage/extracted/$name/overgit" --version)" = "overgit $version"
