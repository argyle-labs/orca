# Test coverage baseline — 2026-05-19

Captured by `cargo llvm-cov --workspace --summary-only --no-fail-fast` on commit
`2a7c884` (v0.0.4-rc.1). This is the floor we ratchet up to 100% per
`project_test_coverage_100.md` in user memory.

## Per-crate

| Crate          | Regions covered | Total regions | Coverage |
|----------------|-----------------|---------------|---------:|
| `tools`        |             4   |             4 | 100.00 % |
| `tools-macro`  |           224   |           277 |  80.87 % |
| `utils`        |         2 459   |         3 147 |  78.14 % |
| `sdk`          |         3 662   |         4 698 |  77.95 % |
| `db`           |         2 181   |         2 842 |  76.74 % |
| `integrations` |         1 723   |         2 774 |  62.11 % |
| `server`       |        12 758   |        38 844 |  32.84 % |
| `tools-def`    |           368   |         3 045 |  12.09 % |
| **TOTAL**      |    **26 554**   |    **59 215** | **44.84 %** |

## Reading the numbers

- The dominant gap is `server` (~26K uncovered regions) and `tools-def` (~2.7K
  uncovered). Together they're ~88% of the climb to 100%.
- `tools-def` percentage is misleading low: the file `tools-def/src/lib.rs`
  scores 100%, but every per-domain file (`host.rs`, `mgmt.rs`, `pod.rs`, etc.)
  is at 0% — these are the `#[orca_tool]` bodies that have no direct unit tests.
  Coverage on the contract layer comes from exercising the tools, not from
  testing the per-file logic in isolation.
- `server` includes axum handlers + main.rs + scheduler + mesh PKI; much of it
  is integration-test-shaped, not unit-test-shaped.

## How to regenerate

```sh
cargo llvm-cov --workspace --summary-only --no-fail-fast
# or with HTML report:
cargo llvm-cov --workspace --html --no-fail-fast --output-dir target/llvm-cov
```

## Next steps

See task #7 (add CI gate that ratchets from this baseline) and
`project_test_coverage_100.md` for the order-of-attack plan.

## Progress log

| Date       | Lines % | Δ      | Slice                                                  |
|------------|--------:|-------:|--------------------------------------------------------|
| 2026-05-19 | 44.98 % |    —   | Baseline.                                              |
| 2026-05-20 | 45.67 % | +0.69  | `tools-def/host.rs` — 11 tests covering all 3 tools.   |
| 2026-05-20 | 46.28 % | +0.61  | tools-def: `meta`, `system` (mock service), `host_status` (5 cases). |
| 2026-05-20 | 46.94 % | +0.66  | tools-def: `plugin_runtime`, `orca_db`, `infra` — stub-service tests. |
| 2026-05-20 | 47.52 % | +0.58  | tools-def: `engine` — `infer_kind` unit + full lifecycle DB tests. |
