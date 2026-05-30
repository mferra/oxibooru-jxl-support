#!/usr/bin/env bash
# Run all Rust tests for oxibooru server.
#
# Prerequisites:
#   - A running PostgreSQL instance reachable by the credentials in ../.env
#     (or override via env vars below).
#   - Rust nightly toolchain installed  (rustup toolchain install nightly)
#
# Usage:
#   cd server && ./test.sh
#   POSTGRES_USER=foo POSTGRES_PASSWORD=bar ./test.sh
#
# The test suite drops and recreates the '__test' database on every run.
# It does NOT touch the database named in POSTGRES_DB.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Load .env from the project root if the caller hasn't already provided vars.
ENV_FILE="$SCRIPT_DIR/../.env"
if [[ -f "$ENV_FILE" ]]; then
  # Export only variables not already set in the environment.
  set -a
  # shellcheck source=/dev/null
  source "$ENV_FILE"
  set +a
fi

: "${POSTGRES_USER:?POSTGRES_USER must be set (in ../.env or environment)}"
: "${POSTGRES_PASSWORD:?POSTGRES_PASSWORD must be set (in ../.env or environment)}"
: "${POSTGRES_DB:?POSTGRES_DB must be set (in ../.env or environment)}"
POSTGRES_HOST="${POSTGRES_HOST:-localhost}"

echo "==> PostgreSQL: ${POSTGRES_USER}@${POSTGRES_HOST}/${POSTGRES_DB}"
echo "==> Test DB:    __test (dropped and recreated each run)"

# Verify PostgreSQL is reachable before spending time compiling.
if command -v pg_isready &>/dev/null; then
  if ! pg_isready -h "$POSTGRES_HOST" -U "$POSTGRES_USER" -q; then
    echo "ERROR: PostgreSQL is not reachable at $POSTGRES_HOST. Is it running?" >&2
    exit 1
  fi
fi

echo "==> Building and running tests (nightly, release-like optimisations disabled)..."
cargo +nightly test \
  --all \
  -- \
  --test-threads=1 \
  "$@"
