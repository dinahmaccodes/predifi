// # Stress Tests & Performance Benchmarks
//
// This module validates the contract's behaviour under high-volume load and
// documents the theoretical storage/gas limits of the current implementation.
//
// ## Storage cost per operation (persistent entries touched)
//
// | Function          | Reads | Writes | Total keys |
// |-------------------|-------|--------|------------|
// | `create_pool`     |   3   |   5    |     8      |
// | `place_prediction`|   4   |   6    |    10      |
// | `resolve_pool`    |   3   |   3    |     6      |
// | `claim_winnings`  |   4   |   2    |     6      |
//
// ## Soroban ledger limits (as of Protocol 22)
//
// * Max persistent entry size : 128 KB
// * Max read entries / tx      : 100
// * Max write entries / tx     : 25
// * Max instructions / tx      : 100 million
//
// ## Theoretical limits
//
// Given the 25-write limit per transaction, `place_prediction` (6 persistent
// write entries) can be called at most ~4 times in a single Soroban invocation
// before saturating the write budget.  In the current design each invocation is
// a separate transaction, so the effective per-pool limit is bounded only by
// the number of unique users (u64 address space) and available token supply.
//
// The `OutcomeStakes(pool_id)` batch vector grows with `options_count`.  At the
// Soroban 128 KB entry cap and 16 bytes per i128 element a pool can support up
// to ~8 000 outcomes before that single entry would exceed the size limit.
// The current `MAX_OPTIONS_COUNT` constant (100) is therefore well within bounds.

#![cfg(test)]
#![allow(deprecated)]

extern crate alloc;

use alloc::vec::Vec as AllocVec;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, Env, String, Symbol,
};

// ── Shared dummy access-control ───────────────────────────────────────────────

mod dummy_access_control_stress {
    use soroban_sdk::{contract, contractimpl, Address, Env, Symbol};

    #[contract]
    pub struct DummyAccessControl;

    #[contractimpl]
    impl DummyAccessControl {
        pub fn grant_role(env: Env, user: Address, role: u32) {
            let key = (Symbol::new(&env, "role"), user, role);
            env.storage().instance().set(&key, &true);
        }

        pub fn has_role(env: Env, user: Address, role: u32) -> bool {
            let key = (Symbol::new(&env, "role"), user, role);
            env.storage().instance().get(&key).unwrap_or(false)
        }
    }
}

const ROLE_ADMIN: u32 = 0;
const ROLE_OPERATOR: u32 = 1;

/// Returns `(client, token_client, token_addr, token_admin, operator, admin)`.
fn stress_setup(
    env: &Env,
) -> (
    PredifiContractClient<'_>,
    token::Client<'_>,
    Address,
    token::StellarAssetClient<'_>,
    Address,
    Address,
) {
    let ac_id = env.register(dummy_access_control_stress::DummyAccessControl, ());
    let ac_client = dummy_access_control_stress::DummyAccessControlClient::new(env, &ac_id);

    let admin = Address::generate(env);
    let operator = Address::generate(env);
    ac_client.grant_role(&admin, &ROLE_ADMIN);
    ac_client.grant_role(&operator, &ROLE_OPERATOR);

    let contract_id = env.register(PredifiContract, ());
    let client = PredifiContractClient::new(env, &contract_id);

    let token_admin_addr = Address::generate(env);
    let token_contract = env.register_stellar_asset_contract(token_admin_addr.clone());
    let token = token::Client::new(env, &token_contract);
    let token_admin_client = token::StellarAssetClient::new(env, &token_contract);

    let treasury = Address::generate(env);
    client.init(&ac_id, &treasury, &0u32, &0u64);
    client.add_token_to_whitelist(&admin, &token_contract);

    (
        client,
        token,
        token_contract,
        token_admin_client,
        operator,
        admin,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress Test 1 – High-volume predictions on a single market
// ─────────────────────────────────────────────────────────────────────────────

/// Simulates 100 unique users each placing a prediction on a single pool.
///
/// Validates:
/// * Pool total stake equals the sum of all individual stakes.
/// * Every winning user can claim independently (no contention on claim state).
/// * The contract balance reaches zero after all claims are processed.
///
/// Storage exercised: 100 × `place_prediction` writes + 100 × `claim_winnings`.
#[test]
fn test_high_volume_predictions_single_pool() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, token, token_addr, token_admin, operator, _admin) = stress_setup(&env);

    let num_users: u32 = 100;
    let stake_per_user: i128 = 100;

    let mut users: AllocVec<Address> = AllocVec::new();
    for _ in 0..num_users {
        let u = Address::generate(&env);
        token_admin.mint(&u, &(stake_per_user * 2));
        users.push(u);
    }

    let creator = Address::generate(&env);
    let pool_id = client.create_pool(
        &creator,
        &100_000u64,
        &token_addr,
        &2u32,
        &String::from_str(&env, "Will the event happen?"),
        &String::from_str(
            &env,
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ),
        &0i128,
        &Symbol::new(&env, "stress"),
    );

    // even-indexed users → outcome 0 (YES), odd → outcome 1 (NO)
    for (i, user) in users.iter().enumerate() {
        let outcome = if i % 2 == 0 { 0u32 } else { 1u32 };
        client.place_prediction(user, &pool_id, &stake_per_user, &outcome);
    }

    let expected_total = i128::from(num_users) * stake_per_user;
    assert_eq!(token.balance(&client.address), expected_total);

    env.ledger().with_mut(|li| li.timestamp = 100_001);
    client.resolve_pool(&operator, &pool_id, &0u32);

    // 50 winners (even indices), 50 losers (odd indices)
    let winner_stake_total = i128::from(num_users / 2) * stake_per_user;
    let mut total_claimed: i128 = 0;

    for (i, user) in users.iter().enumerate() {
        let claimed = client.claim_winnings(user, &pool_id);
        if i % 2 == 0 {
            let expected = stake_per_user * expected_total / winner_stake_total;
            assert_eq!(
                claimed, expected,
                "Winner {i}: expected {expected} got {claimed}"
            );
            total_claimed += claimed;
        } else {
            assert_eq!(claimed, 0, "Loser {i} should get 0 but got {claimed}");
        }
    }

    assert_eq!(token.balance(&client.address), 0);
    assert_eq!(total_claimed, expected_total);
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress Test 2 – Bulk claim winnings
// ─────────────────────────────────────────────────────────────────────────────

/// Places 50 predictions, resolves, then has every user claim.
/// Verifies the double-claim guard holds under bulk pressure and no tokens leak.
#[test]
fn test_bulk_claim_winnings() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, token, token_addr, token_admin, operator, _admin) = stress_setup(&env);

    let num_users: u32 = 48;
    let stake: i128 = 200;

    let mut winners: AllocVec<Address> = AllocVec::new();
    let mut losers: AllocVec<Address> = AllocVec::new();

    for i in 0..num_users {
        let u = Address::generate(&env);
        token_admin.mint(&u, &stake);
        if i % 3 == 0 {
            losers.push(u);
        } else {
            winners.push(u);
        }
    }

    let creator = Address::generate(&env);
    let pool_id = client.create_pool(
        &creator,
        &200_000u64,
        &token_addr,
        &2u32,
        &String::from_str(&env, "Stress bulk claim test"),
        &String::from_str(
            &env,
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ),
        &0i128,
        &Symbol::new(&env, "stress"),
    );

    for w in &winners {
        client.place_prediction(w, &pool_id, &stake, &0u32);
    }
    for l in &losers {
        client.place_prediction(l, &pool_id, &stake, &1u32);
    }

    let total_stake = i128::from(winners.len() as u32 + losers.len() as u32) * stake;
    assert_eq!(token.balance(&client.address), total_stake);

    env.ledger().with_mut(|li| li.timestamp = 200_001);
    client.resolve_pool(&operator, &pool_id, &0u32);

    let winning_pool: i128 = i128::from(winners.len() as u32) * stake;
    let mut total_paid: i128 = 0;
    for w in &winners {
        let payout = client.claim_winnings(w, &pool_id);
        let expected = stake * total_stake / winning_pool;
        assert_eq!(payout, expected);
        total_paid += payout;
    }

    for l in &losers {
        let payout = client.claim_winnings(l, &pool_id);
        assert_eq!(payout, 0);
    }

    assert_eq!(token.balance(&client.address), 0);
    assert_eq!(total_paid, total_stake);
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress Test 3 – Sequential pool creation
// ─────────────────────────────────────────────────────────────────────────────

/// Creates 50 pools in sequence to exercise the `PoolIdCounter` and category
/// index under repeated writes. Verifies each pool ID is unique and sequential.
#[test]
fn test_sequential_pool_creation_stress() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _token, token_addr, _token_admin, _operator, _admin) = stress_setup(&env);

    let creator = Address::generate(&env);
    let num_pools: u32 = 50;
    let mut pool_ids: AllocVec<u64> = AllocVec::new();

    for i in 0..num_pools {
        let pool_id = client.create_pool(
            &creator,
            &(100_000u64 + u64::from(i) * 1_000),
            &token_addr,
            &2u32,
            &String::from_str(&env, "Stress Pool"),
            &String::from_str(
                &env,
                "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
            ),
            &0i128,
            &Symbol::new(&env, "stress"),
        );
        pool_ids.push(pool_id);
    }

    assert_eq!(pool_ids.len(), num_pools as usize);
    for (expected, got) in pool_ids.iter().enumerate() {
        assert_eq!(
            *got, expected as u64,
            "Expected sequential ID {expected} but got {got}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress Test 4 – Multi-outcome pool with high prediction volume
// ─────────────────────────────────────────────────────────────────────────────

/// Creates a pool with 16 outcomes (triggers the `OutcomeStakesUpdatedEvent`
/// batch path) and places 5 predictions on each outcome.
/// Verifies resolution and claim work correctly with many distinct outcome buckets.
///
/// # Storage note
/// At `MAX_OPTIONS_COUNT` = 100 the `OutcomeStakes` vector is
/// 100 × 16 bytes = 1 600 bytes — well under the 128 KB single-entry limit.
#[test]
fn test_max_outcomes_high_volume() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, token, token_addr, token_admin, operator, _admin) = stress_setup(&env);

    let num_outcomes: u32 = 16;
    let num_users_per_outcome: u32 = 5;
    let stake: i128 = 100;

    let creator = Address::generate(&env);
    let pool_id = client.create_pool(
        &creator,
        &500_000u64,
        &token_addr,
        &num_outcomes,
        &String::from_str(&env, "16-outcome tournament"),
        &String::from_str(
            &env,
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ),
        &0i128,
        &Symbol::new(&env, "stress"),
    );

    let mut all_users: AllocVec<(Address, u32)> = AllocVec::new();
    for outcome_idx in 0..num_outcomes {
        for _ in 0..num_users_per_outcome {
            let u = Address::generate(&env);
            token_admin.mint(&u, &stake);
            client.place_prediction(&u, &pool_id, &stake, &outcome_idx);
            all_users.push((u, outcome_idx));
        }
    }

    let total_users = num_outcomes * num_users_per_outcome;
    let total_stake = i128::from(total_users) * stake;
    assert_eq!(token.balance(&client.address), total_stake);

    let winning_outcome: u32 = 0;
    env.ledger().with_mut(|li| li.timestamp = 500_001);
    client.resolve_pool(&operator, &pool_id, &winning_outcome);

    let winning_pool_stake = i128::from(num_users_per_outcome) * stake;
    let expected_per_winner = stake * total_stake / winning_pool_stake;

    let mut claimed_total: i128 = 0;
    for (user, user_outcome) in &all_users {
        let payout = client.claim_winnings(user, &pool_id);
        if *user_outcome == winning_outcome {
            assert_eq!(payout, expected_per_winner);
            claimed_total += payout;
        } else {
            assert_eq!(payout, 0);
        }
    }

    assert_eq!(token.balance(&client.address), 0);
    assert_eq!(claimed_total, total_stake);
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress Test 5 – Throughput measurement
// ─────────────────────────────────────────────────────────────────────────────

/// Validates correctness of 75 sequential `place_prediction` calls on one pool
/// and serves as a baseline for future gas-optimised implementations.
///
/// ## Reference
/// 75 predictions × 6 persistent write keys = 450 total persistent writes.
#[test]
fn test_prediction_throughput_measurement() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, token, token_addr, token_admin, operator, _admin) = stress_setup(&env);

    let n: u32 = 75;
    let stake: i128 = 50;

    let creator = Address::generate(&env);
    let pool_id = client.create_pool(
        &creator,
        &300_000u64,
        &token_addr,
        &3u32,
        &String::from_str(&env, "Throughput benchmark pool"),
        &String::from_str(
            &env,
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ),
        &0i128,
        &Symbol::new(&env, "stress"),
    );

    let mut users: AllocVec<Address> = AllocVec::new();
    for i in 0..n {
        let u = Address::generate(&env);
        token_admin.mint(&u, &stake);
        let outcome = (i % 3) as u32;
        client.place_prediction(&u, &pool_id, &stake, &outcome);
        users.push(u);
    }

    assert_eq!(
        token.balance(&client.address),
        i128::from(n) * stake,
        "Total stake mismatch after {n} predictions"
    );

    env.ledger().with_mut(|li| li.timestamp = 300_001);
    client.resolve_pool(&operator, &pool_id, &0u32);

    let mut claimed: i128 = 0;
    for u in &users {
        claimed += client.claim_winnings(u, &pool_id);
    }

    // Conservation: total claimed == total staked
    assert_eq!(claimed, i128::from(n) * stake);
    assert_eq!(token.balance(&client.address), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress Test 6 – Resolution under load
// ─────────────────────────────────────────────────────────────────────────────

/// Creates 20 pools with 10 predictions each, resolves all, and drains every
/// claim. Verifies pool state is fully isolated and no tokens are lost.
#[test]
fn test_resolution_under_load() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, token, token_addr, token_admin, operator, _admin) = stress_setup(&env);

    let num_pools: u32 = 20;
    let users_per_pool: u32 = 10;
    let stake: i128 = 100;

    let creator = Address::generate(&env);

    let mut pool_ids: AllocVec<u64> = AllocVec::new();
    let mut pool_users: AllocVec<AllocVec<Address>> = AllocVec::new();

    for p in 0..num_pools {
        let pool_id = client.create_pool(
            &creator,
            &(200_000u64 + u64::from(p) * 1_000),
            &token_addr,
            &2u32,
            &String::from_str(&env, "Load Pool"),
            &String::from_str(
                &env,
                "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
            ),
            &0i128,
            &Symbol::new(&env, "stress"),
        );
        pool_ids.push(pool_id);

        let mut users_for_pool: AllocVec<Address> = AllocVec::new();
        for j in 0..users_per_pool {
            let u = Address::generate(&env);
            token_admin.mint(&u, &stake);
            let outcome = if j < users_per_pool / 2 { 0u32 } else { 1u32 };
            client.place_prediction(&u, &pool_id, &stake, &outcome);
            users_for_pool.push(u);
        }
        pool_users.push(users_for_pool);
    }

    let expected_balance = i128::from(num_pools * users_per_pool) * stake;
    assert_eq!(token.balance(&client.address), expected_balance);

    env.ledger().with_mut(|li| li.timestamp = 300_000);

    let mut grand_claimed: i128 = 0;
    for (pool_idx, pool_id) in pool_ids.iter().enumerate() {
        client.resolve_pool(&operator, pool_id, &0u32);

        for user in &pool_users[pool_idx] {
            grand_claimed += client.claim_winnings(user, pool_id);
        }
    }

    assert_eq!(token.balance(&client.address), 0);
    assert_eq!(grand_claimed, expected_balance);
}
