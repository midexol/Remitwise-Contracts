# TODO

- [ ] Add correctness boundary test in `savings_goals/src/test.rs` ensuring `batch_add_to_goals` rejects `MAX_BATCH_SIZE + 1` with `SavingsGoalError::BatchTooLarge`.
- [ ] Add new gas benchmark regression tests in `savings_goals/tests/gas_bench.rs` (or `benchmarks/src/lib.rs`, per repo pattern) for `batch_add_to_goals` at sizes 1/10/25/50, comparing batch CPU+mem against equivalent single `add_to_goal` calls.
- [ ] Update `benchmarks/baseline.json` and `benchmarks/thresholds.json` with new `savings_goals.batch_add_to_goals` scenarios (cpu/mem baselines + appropriate thresholds).
- [ ] Update `SCHEDULE_GAS_BENCHMARKS_SUMMARY.md` (or relevant docs) to include the new savings_goals batch_add_to_goals benchmark results/scenarios.
- [ ] Run `cargo test -p savings_goals --test test` and `cargo test -p savings_goals --test gas_bench` (and any bench-regression harness) to ensure CI passes.

