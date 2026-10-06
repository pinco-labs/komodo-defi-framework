PR #2734 — selected validation evidence, 2026-10-06
https://github.com/GLEECBTC/komodo-defi-framework/pull/2734

Candidate commit: ca7031b9368af50f2f40cedf8183b87ce9865fa7
Upstream dev base: e686ef3500585f01c9f0e89c8c01bc036c42253c
Tested code tree: 56e948699246f5be89d0bd22bd98202dba3a40be
Published commit tree: 58727786d77880274f5ec5c1ddd1201feff0ce66
The operator verified that only docs/plans/mintlayer-integration.md differs
between the tested tree and the published commit. Code content is identical.
Logs were captured before the final commit; no binary identity is claimed.

RESULTS
141 filtered Mintlayer library tests, 1 SDK key adapter test, 5 SDK offline
tests and 2 opt-in API2 live GET tests passed: 149 selected tests, not the
full workspace suite. Commands and exit codes are in candidate-provenance.txt.
API2 live tests validate canonical golden bytes and simulated-primary 503
GET failover. They do not validate POST, a real API1 outage or a new swap.
Historical mainnet swap/runtime evidence is NOT included in this bundle.

CLIPPY BASELINE COMPARISON
Both candidate and unmodified upstream dev ran:
cargo clippy --locked --offline -j 2 -p coins --all-targets --no-deps -- -D warnings
Rust 1.97.1, Cargo 1.97.1; baseline Clippy 0.1.97.
Both exited 101: library 36 errors, library-test 38 errors (overlapping).
The entire error section from the first "error:" through the final
"error: could not compile" line is identical in the original UTF-8 logs.
It contains 38 emitted diagnostic records with identical messages and
locations. No new diagnostics appear in the candidate's observed output.
This demonstrates that these observed failures already occur upstream.
It is NOT a passing Clippy gate, proof all targets completed linting, or
proof of absence of all regressions. Dependency lockfiles differ by design.
Baseline captured at 2026-10-06T12:30:37Z in a clean detached worktree.

FORMATTING / REMAINING GATES
Workspace formatting exited 1 on import ordering in
mm2src/derives/enum_derives/src/from_stringify.rs, verified by the operator
unchanged from dev. Three edited Mintlayer source files were formatted.
Full workspace/Docker, all-feature and multi-platform/native/WASM validation
remain outstanding. CI workflow approval and maintainer review are external
requirements; this bundle does not assert their completion.

PROVENANCE AND TRANSFORMATIONS
The candidate manifest is a chronological record: initial tree387c, patches,
then staged tested tree56e. Its earlier entries are intentionally preserved.
checksums.json records original byte hashes and published byte hashes.
Published copies replace /home/ubuntu/ with <HOME>/ and strip ANSI terminal
control sequences. Filenames are normalized as mapped in checksums.json.
No result lines or diagnostics were removed. Hashes embedded within historical
provenance refer to original files, not the transformed copies in this ZIP.
Only selected logs and manifests are included, not referenced patch backups.
