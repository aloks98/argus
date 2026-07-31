#!/usr/bin/env bash
# Builds the two release tarballs into dist-release/. Used by release.yml
# and runnable locally: scripts/package-release.sh <version>
set -euo pipefail
VERSION="${1:?usage: package-release.sh <version, e.g. 0.1.0>}"
# Resolved from the script location, not git: in CI containers,
# actions/checkout can leave the repo owned by another uid and git then
# refuses with "dubious ownership" -- the script path needs no git at all.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist-release"
rm -rf "$OUT" && mkdir -p "$OUT"

# Server: embedded frontend first, then release build.
echo "==> ensure pnpm"
# No silent fallback: if pnpm is absent AND this install fails (registry
# unreachable, npm broken), the whole run must fail HERE with npm's real
# error -- not three lines later with a bare "pnpm: command not found".
if ! command -v pnpm >/dev/null; then
  npm install -g pnpm@11.17.0
fi
echo "==> frontend install"
pnpm --dir "$ROOT/frontend" install --frozen-lockfile
echo "==> frontend build"
pnpm --dir "$ROOT/frontend" run build
echo "==> server release build"
cargo build --release -p argus-server

# Agent: static musl, asserted.
echo "==> agent musl build"
# Self-heal before target add: files without rustup's manifest (poisoned
# cache, reused runner) make `rustup target add` fail with "detected
# conflict" -- clear strays when rustup doesn't list the target.
if ! rustup target list --installed | grep -q x86_64-unknown-linux-musl; then
  rm -rf "$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-musl"
fi
rustup target add x86_64-unknown-linux-musl
cargo build --release -p argus-agent --target x86_64-unknown-linux-musl
AGENT_BIN="$ROOT/target/x86_64-unknown-linux-musl/release/argus-agent"
file "$AGENT_BIN" | grep -qi 'static' || { echo "ERROR: agent not static"; exit 1; }

stage() { # name, binary, unit, env_example, install_sh
  local dir="$OUT/$1-$VERSION"
  mkdir -p "$dir"
  cp "$2" "$dir/"
  cp "$ROOT/deploy/packaging/$3" "$dir/"
  cp "$ROOT/deploy/packaging/$4" "$dir/"
  cp "$ROOT/deploy/packaging/$5" "$dir/install.sh"
  chmod +x "$dir/install.sh"
  tar -C "$OUT" -czf "$OUT/$1-v$VERSION-$6.tar.gz" "$(basename "$dir")"
  rm -rf "$dir"
}
stage argus-server "$ROOT/target/release/argus" argus.service argus.env.example install-server.sh x86_64-linux-gnu
stage argus-agent  "$AGENT_BIN" argus-agent.service agent.env.example install-agent.sh x86_64-linux-musl
( cd "$OUT" && sha256sum ./*.tar.gz > SHA256SUMS )
ls -la "$OUT"
