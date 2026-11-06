#!/usr/bin/env bash
# Build the Linux cd-copy-save CLI from this repo + vendored crimson-rs.
# Does not write to upstream GitHub. Does not package private saves.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VENDOR="$ROOT/vendor/crimson-rs"
CRIMSON_RS_URL="${CRIMSON_RS_URL:-https://github.com/bbfox0703/crimson-rs.git}"
CRIMSON_RS_REV="${CRIMSON_RS_REV:-8e847047228027d0b1dcaccee79f242f2230e027}"

if [[ ! -d "$VENDOR/.git" ]]; then
  echo "cloning crimson-rs into vendor/ (gitignored)..."
  git clone -- "$CRIMSON_RS_URL" "$VENDOR"
fi
git -C "$VENDOR" fetch --depth 1 origin "$CRIMSON_RS_REV" || git -C "$VENDOR" fetch origin
git -C "$VENDOR" checkout --detach "$CRIMSON_RS_REV"

# Local-only link patches so this CLI can use the save module as an rlib.
# vendor/ is gitignored; these never land in the parent repo or upstream.
export VENDOR
python3 - <<'PY'
from pathlib import Path
import os
vendor = Path(os.environ["VENDOR"])
p = vendor / "Cargo.toml"
t = p.read_text()
t = t.replace('crate-type = ["cdylib", "staticlib"]',
              'crate-type = ["rlib", "cdylib", "staticlib"]', 1)
p.write_text(t)
p = vendor / "src" / "lib.rs"
t = p.read_text()
t = t.replace("mod save;", "pub mod save;", 1)
p.write_text(t)
print("applied local rlib + pub mod save patches")
PY

export VENDOR
cd "$ROOT/tools/copy_save_cli"
cargo build --release
BIN="$ROOT/tools/copy_save_cli/target/release/cd-copy-save"
echo "built $BIN"
sha256sum "$BIN"
"$BIN" --help | head -40
