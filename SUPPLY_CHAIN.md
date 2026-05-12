# Supply chain defense

This is a tracking fork of [rtk-ai/rtk](https://github.com/rtk-ai/rtk). RTK
installs a `PreToolUse` hook on every Bash command Claude Code executes, so any
compromise of the binary has broad blast radius. This document records how we
defend against that.

## Threat model

We assume:

- The upstream maintainer is currently trustworthy, but their account or CI
  could be compromised in the future
- Any dependency in `Cargo.toml` is a separate trust point that could itself
  be compromised (typosquats, account takeovers, malicious updates)
- `build.rs` runs arbitrary code at compile time with full user privileges
- Release binaries on GitHub Releases could differ from source

We do **not** assume:

- The current `HEAD` is malicious (we audited it manually before forking)
- Every upstream change is hostile (most are not — but we verify each one)

## Defenses in place

1. **Built from source.** We never download release binaries. Use
   `cargo install --path .` from this checkout.
2. **No outbound network calls.** The original telemetry module was deleted
   (commit `fe25594`). The `ureq` and `getrandom` dependencies were removed.
3. **Pre-merge scanner.** `scripts/audit-upstream.sh` runs against
   `merge-base..upstream/master` and exits non-zero if any high-risk pattern
   appears: new dependencies, `build.rs` changes, network calls, env reads,
   process spawns, CI workflow edits, removal of safety lints, or `unsafe`
   blocks.
4. **Weekly automated scan.** `.github/workflows/upstream-scan.yml` runs the
   scanner every Sunday 04:00 UTC and opens an issue on this fork if anything
   is flagged.
5. **`cargo audit`** for known CVEs in transitive deps (when installed).
6. **`unsafe_code = "deny"`** at the workspace level. Removal of this lint is
   flagged by the scanner.
7. **No `--no-verify` on commits, no skipped hooks.**

## Merge workflow

Never run `git pull upstream master` directly. Instead:

```bash
# 1. Run the scanner — fetches upstream automatically
bash scripts/audit-upstream.sh

# 2. If exit code is 0, proceed
git merge upstream/master

# 3. Run the test suite end-to-end before recording trust
cargo fmt --all && cargo clippy --all-targets && cargo test --all

# 4. Add an entry to the trust log below
```

If the scanner exits with code 2, **stop and review manually**. Read every
flagged line. Only proceed when satisfied that each flag is benign (e.g., a
new genuinely-useful command filter that legitimately spawns a process).

## Trust log

Each entry records an upstream merge that was reviewed and accepted.

| Date       | Upstream SHA | Upstream tag | Reviewer | Notes                                                                 |
| ---------- | ------------ | ------------ | -------- | --------------------------------------------------------------------- |
| 2026-05-12 | `2fbc7514`   | v0.39.0      | Gh05d    | Initial trust anchor. Source audit cleared (see commit `fe25594`).    |

## What to do if the scanner flags something legitimate

Some upstream changes look suspicious but are benign — e.g., a new
`Command::new("docker")` to support a docker filter. If you decide the change
is safe:

1. Document the rationale in the trust log entry
2. Merge upstream
3. If you expect similar changes regularly (e.g., new filters that all
   spawn processes), narrow the scanner regex rather than disabling it

## What to do if the scanner flags something hostile

1. Do not merge upstream
2. Pin to the last trusted SHA (currently `2fbc7514`)
3. Cherry-pick only the changes you do want
4. Consider reporting the issue upstream
