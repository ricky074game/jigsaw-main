#!/bin/bash
# shellcheck disable=SC2164
cd "${0%/*}"
echo "Building release..."
sudo apt-get update && sudo apt-get install -y libudev-dev

cargo install trunk
trunk build --release
rustup target add wasm32-unknown-unknown
cargo build --release --bin server
rm -f jigsaw.zip
zip jigsaw.zip -j target/release/server run.sh
zip jigsaw.zip -r dist/
unzip jigsaw.zip -d /tmp/jigsaw
