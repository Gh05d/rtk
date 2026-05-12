#!/usr/bin/env bash
# Supply-chain scanner for upstream pulls.
#
# Compares upstream/master against our merge-base and fails (exit 2)
# if any high-risk pattern appears in the diff: new dependencies,
# build.rs changes, network calls, env reads, process spawns, CI
# workflow edits, or removal of safety lints.
#
# Exit codes:
#   0  upstream has nothing new, OR diff is clean of high-risk patterns
#   1  upstream not reachable, git error, or no upstream remote
#   2  high-risk patterns detected — manual review required
#
# Usage: bash scripts/audit-upstream.sh [--no-fetch]

set -u
set -o pipefail

cd "$(git rev-parse --show-toplevel)"

RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

flag() { printf "${RED}[FLAG]${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$1"; }
info() { printf "${BLUE}[INFO]${NC} %s\n" "$1"; }
ok()   { printf "${GREEN}[ OK ]${NC} %s\n" "$1"; }

FLAGS=0

if ! git remote get-url upstream >/dev/null 2>&1; then
    warn "no 'upstream' remote configured; add with: git remote add upstream https://github.com/rtk-ai/rtk.git"
    exit 1
fi

if [ "${1:-}" != "--no-fetch" ]; then
    info "fetching upstream..."
    if ! git fetch upstream --quiet; then
        warn "git fetch upstream failed"
        exit 1
    fi
fi

BASE=$(git merge-base HEAD upstream/master 2>/dev/null || true)
TIP=$(git rev-parse upstream/master 2>/dev/null || true)

if [ -z "$BASE" ] || [ -z "$TIP" ]; then
    warn "could not resolve merge-base or upstream tip"
    exit 1
fi

if [ "$BASE" = "$TIP" ]; then
    ok "upstream is already merged (base = tip = ${TIP:0:8})"
    exit 0
fi

COMMITS_AHEAD=$(git rev-list --count "$BASE".."$TIP")
FILES_CHANGED=$(git diff --name-only "$BASE" "$TIP" | wc -l)
info "upstream has $COMMITS_AHEAD new commits, $FILES_CHANGED files changed"
info "review range: ${BASE:0:8}..${TIP:0:8}"
echo

# ── 1. Cargo.toml deps ──────────────────────────────────────────────
info "scanning Cargo.toml for new dependencies..."
META_KEYS='name|version|edition|authors|description|license|license-file|homepage|repository|readme|keywords|categories|exclude|include|publish|workspace|members|maintainer|copyright|extended-description|section|priority|assets|source|dest|mode|opt-level|lto|codegen-units|panic|strip|debug|incremental'
ADDED_DEPS=$(git diff "$BASE" "$TIP" -- Cargo.toml \
    | grep -E '^\+[a-z][a-zA-Z0-9_-]*\s*=' \
    | grep -vE "^\+($META_KEYS)\s*=" \
    | grep -v '^+++' || true)
if [ -n "$ADDED_DEPS" ]; then
    flag "new/changed dependency lines in Cargo.toml:"
    echo "$ADDED_DEPS" | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# ── 2. Cargo.lock — count net transitive crate changes ─────────────
info "scanning Cargo.lock for transitive crate changes..."
NEW_CRATES=$(git diff "$BASE" "$TIP" -- Cargo.lock | grep -E '^\+name = ' | sort -u | wc -l)
if [ "$NEW_CRATES" -gt 0 ]; then
    warn "Cargo.lock added $NEW_CRATES crate entries — review for typosquats"
fi

# ── 3. build.rs ─────────────────────────────────────────────────────
info "scanning build.rs..."
if git diff --name-only "$BASE" "$TIP" | grep -q '^build\.rs$'; then
    flag "build.rs changed — compile-time code runs with user privileges:"
    git diff "$BASE" "$TIP" -- build.rs | head -30 | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# ── 4. Safety lints in Cargo.toml ───────────────────────────────────
info "scanning for removal of safety lints..."
if git diff "$BASE" "$TIP" -- Cargo.toml | grep -E '^-.*(unsafe_code|warnings)\s*=\s*"deny"' >/dev/null; then
    flag "a [lints] = \"deny\" rule was removed from Cargo.toml"
    FLAGS=$((FLAGS + 1))
fi

# ── 5. CI workflows ─────────────────────────────────────────────────
info "scanning .github/ for CI changes..."
CI_FILES=$(git diff --name-only "$BASE" "$TIP" -- '.github/' | head -10)
if [ -n "$CI_FILES" ]; then
    flag ".github/ files changed — CI compromise = release compromise:"
    echo "$CI_FILES" | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# ── 6. Suspicious Rust patterns in added lines ──────────────────────
info "scanning Rust diff for suspicious patterns..."

# Network egress
NET_RE='ureq|reqwest|hyper|TcpStream|TcpListener|UdpSocket|tokio::net|attohttpc|isahc|surf|http_client'
NET_HITS=$(git diff "$BASE" "$TIP" -- '*.rs' | grep -E "^\+.*($NET_RE)" | grep -vE "^\+\+\+|^\+\s*//|^\+\s*///" || true)
if [ -n "$NET_HITS" ]; then
    flag "new network egress patterns:"
    echo "$NET_HITS" | head -20 | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# Env reads (excluding test config + RTK_DB_PATH which is documented)
ENV_RE='env::var|option_env!|\benv!\('
ENV_HITS=$(git diff "$BASE" "$TIP" -- '*.rs' | grep -E "^\+.*($ENV_RE)" | grep -vE "^\+\+\+|^\+\s*//|^\+\s*///|RTK_DB_PATH|CARGO_PKG_VERSION|OUT_DIR" || true)
if [ -n "$ENV_HITS" ]; then
    flag "new env variable reads (not RTK_DB_PATH/CARGO/OUT_DIR):"
    echo "$ENV_HITS" | head -20 | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# Process spawn (excluding tests and existing patterns)
PROC_RE='Command::new|process::Command'
PROC_HITS=$(git diff "$BASE" "$TIP" -- '*.rs' | grep -E "^\+.*($PROC_RE)" | grep -vE "^\+\+\+|^\+\s*//|tests/|#\[cfg\(test\)\]" || true)
if [ -n "$PROC_HITS" ]; then
    warn "new Command::new spawns (verify target binary):"
    echo "$PROC_HITS" | head -15 | sed 's/^/    /'
fi

# Unsafe blocks (despite global deny — could be removed via cfg attr)
UNSAFE_HITS=$(git diff "$BASE" "$TIP" -- '*.rs' | grep -E '^\+.*(unsafe\s*\{|unsafe fn|allow\(unsafe_code\))' | grep -v '^+++' || true)
if [ -n "$UNSAFE_HITS" ]; then
    flag "new unsafe blocks or allow(unsafe_code):"
    echo "$UNSAFE_HITS" | head -10 | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# Install script changes
if git diff --name-only "$BASE" "$TIP" | grep -qE '^(install\.sh|scripts/install)'; then
    flag "install script changed:"
    git diff "$BASE" "$TIP" -- install.sh | head -40 | sed 's/^/    /'
    FLAGS=$((FLAGS + 1))
fi

# Hook installation logic
HOOK_FILES=$(git diff --name-only "$BASE" "$TIP" | grep -E '^src/hooks/(init|integrity|trust)\.rs' || true)
if [ -n "$HOOK_FILES" ]; then
    warn "hook installation/integrity logic changed (writes to ~/.claude/settings.json):"
    echo "$HOOK_FILES" | sed 's/^/    /'
fi

echo

# ── 7. cargo audit (CVE scan) ───────────────────────────────────────
if command -v cargo-audit >/dev/null 2>&1; then
    info "running cargo audit for known CVEs..."
    if ! cargo audit --quiet 2>&1 | tail -20; then
        flag "cargo audit reported issues"
        FLAGS=$((FLAGS + 1))
    fi
else
    warn "cargo-audit not installed; skipping CVE scan (cargo install cargo-audit)"
fi

echo
if [ "$FLAGS" -eq 0 ]; then
    ok "no high-risk patterns detected in upstream diff"
    info "you may proceed with: git merge upstream/master"
    exit 0
else
    flag "$FLAGS high-risk pattern(s) detected — MANUAL REVIEW REQUIRED"
    info "review the full diff with:"
    info "    git diff $BASE $TIP"
    info "after review, record the new trust anchor in SUPPLY_CHAIN.md"
    exit 2
fi
