#!/usr/bin/env bash
# Run all Rust tests for oxibooru server.
#
# Prerequisites:
#   - A running PostgreSQL instance reachable by the credentials in ../.env
#     (or override via env vars below).
#   - Rust toolchain (nightly preferred; falls back to the active toolchain if
#     rustup is not available).
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

# Load .env from the project root; env vars already in the environment take priority.
ENV_FILE="$SCRIPT_DIR/../.env"
if [[ -f "$ENV_FILE" ]]; then
  while IFS='=' read -r key value; do
    # Skip comments and blank lines.
    [[ "$key" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${key// /}" ]] && continue
    # Strip inline comments and surrounding whitespace from value.
    value="${value%%#*}"
    value="${value#"${value%%[! ]*}"}"
    value="${value%"${value##*[! ]}"}"
    # Only export if the variable is not already set.
    if [[ -z "${!key+x}" ]]; then
      export "$key"="$value"
    fi
  done < "$ENV_FILE"
fi

: "${POSTGRES_USER:?POSTGRES_USER must be set (in ../.env or environment)}"
: "${POSTGRES_PASSWORD:?POSTGRES_PASSWORD must be set (in ../.env or environment)}"
: "${POSTGRES_DB:?POSTGRES_DB must be set (in ../.env or environment)}"
POSTGRES_HOST="${POSTGRES_HOST:-localhost}"

export POSTGRES_USER POSTGRES_PASSWORD POSTGRES_DB POSTGRES_HOST

echo "==> PostgreSQL: ${POSTGRES_USER}@${POSTGRES_HOST}/${POSTGRES_DB}"
echo "==> Test DB:    __test (dropped and recreated each run)"

# Verify PostgreSQL is reachable before spending time compiling.
if command -v pg_isready &>/dev/null; then
  if ! pg_isready -h "$POSTGRES_HOST" -U "$POSTGRES_USER" -q; then
    echo "ERROR: PostgreSQL is not reachable at $POSTGRES_HOST. Is it running?" >&2
    exit 1
  fi
fi

# Use nightly if rustup is available; otherwise use the active toolchain.
if command -v rustup &>/dev/null && rustup toolchain list | grep -q nightly; then
  CARGO_TOOLCHAIN="+nightly"
else
  CARGO_TOOLCHAIN=""
  echo "==> WARNING: rustup/nightly not found — using default toolchain ($(rustc --version 2>/dev/null || echo unknown))"
fi

echo "==> Building and running tests..."
# shellcheck disable=SC2086
cargo $CARGO_TOOLCHAIN test \
  --all \
  -- \
  --test-threads=1 \
  "$@"
