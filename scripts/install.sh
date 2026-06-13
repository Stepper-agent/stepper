#!/usr/bin/env bash
# Build the release `stepper` binary and install it onto your PATH.
#
#   scripts/install.sh                         # installs to ~/.local/bin
#   STEPPER_INSTALL_DIR=/usr/local/bin sudo -E scripts/install.sh
#
# In GitHub Actions the install dir is appended to $GITHUB_PATH so later steps
# can call `stepper` directly.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> building stepper (release, locked)"
cargo build --release --locked --bin stepper

INSTALL_DIR="${STEPPER_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"
install -m 0755 "target/release/stepper" "$INSTALL_DIR/stepper"
echo "==> installed: $INSTALL_DIR/stepper"
"$INSTALL_DIR/stepper" --version

# Expose the install dir to subsequent GitHub Actions steps.
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$INSTALL_DIR" >> "$GITHUB_PATH"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "note: add '$INSTALL_DIR' to your PATH to run 'stepper' directly" ;;
esac
