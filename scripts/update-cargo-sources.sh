#!/bin/sh
# Regenerates build-aux/cargo-sources.json from Cargo.lock, so the Flatpak
# build can fetch every crate before building offline. Run after any
# change to Cargo.lock. Needs Python with aiohttp and tomlkit.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sSLo "$tmp/gen.py" \
	https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
python3 "$tmp/gen.py" "$root/Cargo.lock" -o "$root/build-aux/cargo-sources.json"
