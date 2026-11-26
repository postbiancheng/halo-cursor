#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
NAME="$(sed -n 's/^ *"id": "\(.*\)".*/\1/p' pack.json | head -1)"
OUT_DIR="${ROOT}/dist"
WASM_SRC="target/wasm32-unknown-unknown/release/agent2api_halo_cursor.wasm"
WASM_DST="${OUT_DIR}/halo-cursor.wasm"
ZIP="${OUT_DIR}/${NAME}-${VERSION}.zip"

# Prefer the rustup toolchain binaries. Homebrew rust on PATH often lacks wasm32 std.
if command -v rustup >/dev/null 2>&1; then
  rustup target add wasm32-unknown-unknown >/dev/null
  export RUSTC
  export CARGO
  RUSTC="$(rustup which rustc)"
  CARGO="$(rustup which cargo)"
  TOOLCHAIN="$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')"
  if [[ -n "${TOOLCHAIN}" && -x "${HOME}/.rustup/toolchains/${TOOLCHAIN}/bin/cargo" ]]; then
    RUSTC="${HOME}/.rustup/toolchains/${TOOLCHAIN}/bin/rustc"
    CARGO="${HOME}/.rustup/toolchains/${TOOLCHAIN}/bin/cargo"
  fi
elif [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
  CARGO="${HOME}/.cargo/bin/cargo"
  RUSTC="${HOME}/.cargo/bin/rustc"
else
  CARGO="cargo"
fi

"$CARGO" build --release --target wasm32-unknown-unknown

mkdir -p "$OUT_DIR"
cp -f "$WASM_SRC" "$WASM_DST"
cp -f pack.json wasm.json "$OUT_DIR/"
rm -rf "${OUT_DIR}/assets"
cp -R assets "${OUT_DIR}/assets"

(cd "$OUT_DIR" && zip -q -X -r "$(basename "$ZIP")" pack.json wasm.json halo-cursor.wasm assets)

echo "built: $ZIP"
ls -lh "$ZIP" "$WASM_DST"
