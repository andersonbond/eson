#!/usr/bin/env bash
# Build the macOS one-click installer:
#   1. Compile eson-agent + eson-memory in --release.
#   2. Copy them into apps/desktop/src-tauri/binaries/ with the
#      Tauri sidecar naming convention `<name>-<rust-target-triple>`.
#   3. Run `npm run tauri build` (with CI unset to dodge Tauri's strict
#      `--ci` flag parsing).
#
# Result: apps/desktop/src-tauri/target/release/bundle/dmg/Eson_<v>_<arch>.dmg
# That DMG contains Eson.app with both sidecars + persona/ + skills/
# bundled as resources, so dragging it to /Applications is the entire setup.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# Detect the host's Rust target triple (e.g. aarch64-apple-darwin).
TARGET="$(rustc -vV | awk '/^host:/ {print $2}')"
if [[ -z "${TARGET}" ]]; then
  echo "fatal: could not detect Rust host triple from \`rustc -vV\`" >&2
  exit 1
fi
echo "==> target triple: ${TARGET}"

echo "==> cargo build --release -p eson-agent -p eson-memory"
cargo build --release -p eson-agent -p eson-memory

BIN_DIR="${ROOT}/apps/desktop/src-tauri/binaries"
mkdir -p "${BIN_DIR}"

for name in eson-agent eson-memory; do
  src="${ROOT}/target/release/${name}"
  dst="${BIN_DIR}/${name}-${TARGET}"
  if [[ ! -x "${src}" ]]; then
    echo "fatal: missing release binary ${src}" >&2
    exit 1
  fi
  cp -f "${src}" "${dst}"
  chmod +x "${dst}"
  echo "==> sidecar staged: ${dst}"
done

# Stage persona/ + skills/ inside src-tauri so Tauri's bundler can pick them
# up via simple relative globs (the `../../../` form drops them under
# `_up_/_up_/_up_/` in the Resources dir, which the runtime can't find).
RES_DIR="${ROOT}/apps/desktop/src-tauri/resources"
rm -rf "${RES_DIR}"
mkdir -p "${RES_DIR}"
cp -R "${ROOT}/persona" "${RES_DIR}/persona"
cp -R "${ROOT}/skills"  "${RES_DIR}/skills"
echo "==> resources staged: ${RES_DIR}/{persona,skills}"

echo "==> npm install (apps/desktop)"
( cd apps/desktop && npm install )

echo "==> tauri build"
( cd apps/desktop && env -u CI npm run tauri build )

echo
echo "Done. Artifacts:"
find "${ROOT}/target/release/bundle" -maxdepth 3 -type f \( -name '*.dmg' -o -name 'Info.plist' \) -print
