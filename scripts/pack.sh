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

# Resolve Cargo and rustc together from the toolchain declared in rust-toolchain.toml.
RUSTUP_BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
if [[ -x "$RUSTUP_BIN_DIR/rustup" ]]; then
  export PATH="$RUSTUP_BIN_DIR:$PATH"
fi
if ! command -v rustup >/dev/null 2>&1; then
  echo "rustup is required to build the toolchain declared in rust-toolchain.toml." >&2
  exit 1
fi

rustup toolchain install --no-update --no-self-update
CARGO_BIN="$(rustup which cargo)"
RUSTC="$(rustup which rustc)" "$CARGO_BIN" build --locked --release \
  --target wasm32-unknown-unknown --target-dir "$ROOT/target"

mkdir -p "$OUT_DIR"
cp -f "$WASM_SRC" "$WASM_DST"
cp -f pack.json wasm.json "$OUT_DIR/"
rm -rf "${OUT_DIR}/assets"
cp -R assets "${OUT_DIR}/assets"

PACK_TMP="$(mktemp -d "${OUT_DIR}/.pack.XXXXXX")"
trap 'rm -rf "$PACK_TMP"' EXIT
(cd "$OUT_DIR" && zip -q -X -r "$PACK_TMP/$(basename "$ZIP")" pack.json wasm.json halo-cursor.wasm assets)
mv -f "$PACK_TMP/$(basename "$ZIP")" "$ZIP"
(
  cd "$OUT_DIR"
  shasum -a 256 "$(basename "$ZIP")" > "$(basename "$ZIP").sha256"
)
{
  printf 'git_commit=%s\n' "$(git rev-parse HEAD)"
  if [[ -n "$(git status --porcelain)" ]]; then
    printf 'worktree=dirty\n'
  else
    printf 'worktree=clean\n'
  fi
  printf 'wasm_sha256=%s\n' "$(shasum -a 256 "$WASM_DST" | awk '{print $1}')"
} > "${ZIP}.build-info"

echo "built: $ZIP"
ls -lh "$ZIP" "$WASM_DST"
