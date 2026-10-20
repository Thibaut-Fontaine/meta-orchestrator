#!/usr/bin/env bash
# Build mox (the meta-orchestrator) the easy way.
#
#   ./build.sh                 # release build
#   ./build.sh --test          # format + clippy + tests + release build
#   ./build.sh --debug         # fast debug build
#   ./build.sh --install       # build and install to ~/.local/bin/mox
#   ./build.sh --offline       # use only the local cargo cache
#
# No CPU benchmark, no project code is run: this only builds mox itself.
set -euo pipefail
cd "$(dirname "$0")"

PROFILE="release"
RUN_TESTS=0
INSTALL=0
CARGO_OFFLINE=()

usage() {
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
}

for arg in "$@"; do
  case "$arg" in
    --debug) PROFILE="debug" ;;
    --test|--tests) RUN_TESTS=1 ;;
    --install) INSTALL=1 ;;
    --offline) CARGO_OFFLINE=(--offline) ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is not installed." >&2
  echo "Install Rust from https://rustup.rs, then re-run ./build.sh" >&2
  exit 1
fi

echo "==> mox build (profile: $PROFILE)"
cargo --version

if [[ $RUN_TESTS -eq 1 ]]; then
  if cargo fmt --version >/dev/null 2>&1; then
    echo "==> cargo fmt --check"
    cargo fmt --check
  else
    echo "warning: rustfmt not installed, skipping format check" >&2
  fi

  if cargo clippy --version >/dev/null 2>&1; then
    echo "==> cargo clippy"
    cargo clippy "${CARGO_OFFLINE[@]}" --all-targets -- -D warnings
  else
    echo "warning: clippy not installed, skipping lint" >&2
  fi

  echo "==> cargo test"
  cargo test "${CARGO_OFFLINE[@]}"
fi

echo "==> cargo build ($PROFILE)"
if [[ "$PROFILE" == "release" ]]; then
  cargo build "${CARGO_OFFLINE[@]}" --release
  BIN="target/release/mox"
else
  cargo build "${CARGO_OFFLINE[@]}"
  BIN="target/debug/mox"
fi

echo "==> built $BIN"
"$BIN" --version

if [[ $INSTALL -eq 1 ]]; then
  DEST="${HOME}/.local/bin"
  mkdir -p "$DEST"
  install -m 755 "$BIN" "$DEST/mox"
  echo "==> installed $DEST/mox"
  case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "note: add $DEST to PATH to run 'mox' from anywhere" ;;
  esac
fi

cat <<'NEXT'

Next steps:
  mox init                 # write config + create ~/mox-projects
  mox doctor               # check opencode, gh, git, disk, VPS
  mox plan --ideas ideas.example.txt
  mox up
  mox status --watch       # live milestone progress and ETA
NEXT
