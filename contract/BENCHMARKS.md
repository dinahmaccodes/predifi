# Performance Benchmarks & Stress Test Specification

This document records the theoretical and measured performance characteristics
of the `predifi-contract` Soroban smart contract.

---

## Running the Stress Tests

The stress tests live in `contracts/predifi-contract/src/stress_test.rs` and are
compiled only in test mode (zero impact on the production WASM binary).

```bash
cd contract

# Run all stress tests
cargo test stress

# Run with println output visible (for throughput comments)
cargo test stress -- --nocapture

# Run the full test suite (including unit + integration + stress)
cargo test
```

---

## Storage Cost Per Operation

Each Soroban persistent-storage entry counts as one read and one write within a
transaction's budget. The table below lists the **persistent** entries touched by
the four hot-path functions in the current implementation.

| Function            | Reads | Writes | Net distinct keys |
|---------------------|------:|-------:|------------------:|
| `create_pool`       |   3   |   5    |        5          |
| `place_prediction`  |   4   |   6    |        6          |
| `resolve_pool`      |   3   |   3    |        3          |
| `claim_winnings`    |   4   |   2    |        4          |

### Keys written by `place_prediction`

| Key                              | Purpose                                    |
|----------------------------------|--------------------------------------------|
| `Prediction(user, pool_id)`      | Stores the user's stake and chosen outcome |
| `Pool(pool_id)`                  | Updates `total_stake`                      |
| `OutcomeStakes(pool_id)`         | Batch-vector: all outcome stakes at once   |
| `OutcomeStake(pool_id, outcome)` | Legacy individual key (backward compat.)   |
| `UserPredictionIndex(user, n)`   | Index entry for pagination                 |
| `UserPredictionCount(user)`      | Prediction count for the user              |

---

## Soroban Ledger Limits (Protocol 22)

| Limit                                      | Value       |
|--------------------------------------------|-------------|
| Max persistent entry size                  | 128 KB      |
| Max read ledger entries per transaction    | 100         |
| Max write ledger entries per transaction   | 25          |
| Max Wasm instructions per transaction      | 100 million |
| Max contract WASM binary size              | 256 KB      |

---

## Theoretical Limits

### Predictions per pool
Each `place_prediction` call is a **separate** Soroban transaction, so the
write-entries-per-transaction limit does not constrain the total number of
predictions across multiple transactions. The practical upper bounds are:

* **Unique bettors**: theoretically unlimited (bounded only by the token supply
  and the i128 `total_stake` overflow boundary of ~1.7 × 10³⁸ tokens).
* **Same-user repeated bets**: the current implementation stores only the
  *latest* prediction per `(user, pool_id)` pair. A second call from the same
  user overwrites the previous entry, so user deduplication is implicit in
  storage.

### Outcome stakes vector
`OutcomeStakes(pool_id)` stores a `Vec<i128>` with one element per outcome.
At 16 bytes per `i128`:

| Options count | Vector size |
|---------------|-------------|
| 2             | 32 B        |
| 10            | 160 B       |
| 100 (MAX)     | 1.6 KB      |
| ≤ 8 192       | 128 KB cap  |

The current `MAX_OPTIONS_COUNT = 100` is **well within** the entry-size limit.
The constant can safely be raised to ~8 000 before the batch-vector approach
requires a different storage layout (e.g., chunked vectors).

### Pool ID counter
The counter is a `u64`, supporting up to **18.4 × 10¹⁸** pools before
overflow—effectively unlimited.

---

## Stress Test Results (Soroban Test Environment)

The Soroban test runner executes contract calls in-process without network
overhead, so wall-clock numbers reflect pure compute cost.

| Test                                      | Users/Pools | Operations  | Outcome                    |
|-------------------------------------------|------------|-------------|----------------------------|
| `test_high_volume_predictions_single_pool`| 100 users  | 100 pred.   | ✅ Pass, all claims correct |
| `test_bulk_claim_winnings`                | 50 users   | 50 pred.    | ✅ Pass, no token leakage   |
| `test_sequential_pool_creation_stress`    | 1 creator  | 50 pools    | ✅ Pass, IDs sequential     |
| `test_max_outcomes_high_volume`           | 80 users   | 80 pred.    | ✅ Pass, 16-outcome market  |
| `test_prediction_throughput_measurement`  | 75 users   | 75 pred.    | ✅ Pass, stake conserved    |
| `test_resolution_under_load`              | 200 users  | 20 pools    | ✅ Pass, no cross-pool leak |

---

## Recommendations for Future Optimisation

1. **Batch user prediction index**: Replace the linked-list style
   `UserPredictionIndex(user, n)` with a single `Vec<u64>` stored under one key
   to halve the per-prediction write count.
2. **Lazy TTL bumps**: Defer `extend_ttl` calls to resolve-time for pool data
   (read TTL only needs to outlast the betting window).
3. **Fee collection batching**: Accumulate fees per-pool and sweep to treasury
   on `resolve_pool` rather than on each `place_prediction`, reducing per-call
   write count by one.
4. **Cap same-user re-predictions**: Allowing repeated predictions from the same
   address (overwriting) silently discards prior stakes. Consider an explicit
   accumulate-or-reject policy and document it in the pool spec.
