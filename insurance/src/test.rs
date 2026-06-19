use crate::{Insurance, InsuranceClient, InsuranceError};
use remitwise_common::CoverageType;
use soroban_sdk::{testutils::Address as _, Address, Env, String, Vec};

fn fresh_env() -> (Env, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, Insurance);
    (env, contract_id)
}

fn init_contract(client: &InsuranceClient<'_>, env: &Env) -> Address {
    let owner = Address::generate(&env);
    client.init(&owner);
    owner
}

fn policy_name(env: &Env) -> String {
    String::from_str(env, "Family Cover")
}

fn external_ref(env: &Env) -> String {
    String::from_str(env, "ext-001")
}

/// `init` is single-shot: the first call succeeds and a second call cannot
/// replace the configured owner.
#[test]
fn test_init_is_single_shot() {
    let (env, contract_id) = fresh_env();
    let client = InsuranceClient::new(&env, &contract_id);
    let owner = Address::generate(&env);
    let attacker = Address::generate(&env);

    client.init(&owner);
    let second = client.try_init(&attacker);
    assert_eq!(second, Err(Ok(InsuranceError::AlreadyInitialized)));

    let policy_owner = Address::generate(&env);
    let policy_id = client.create_policy(
        &policy_owner,
        &policy_name(&env),
        &CoverageType::Health,
        &100,
        &10_000,
    );

    assert_eq!(
        client.set_external_ref(&owner, &policy_id, &Some(external_ref(&env))),
        true
    );
    assert_eq!(
        client.try_set_external_ref(&attacker, &policy_id, &None),
        Err(Ok(InsuranceError::Unauthorized))
    );
}

/// Every mutating entrypoint rejects pre-init access with `NotInitialized`.
#[test]
fn test_mutators_reject_before_init() {
    let (env, contract_id) = fresh_env();
    let client = InsuranceClient::new(&env, &contract_id);
    let caller = Address::generate(&env);
    let ids = Vec::new(&env);

    assert_eq!(
        client.try_create_policy(
            &caller,
            &policy_name(&env),
            &CoverageType::Health,
            &100,
            &10_000,
        ),
        Err(Ok(InsuranceError::NotInitialized))
    );
    assert_eq!(
        client.try_pay_premium(&caller, &1),
        Err(Ok(InsuranceError::NotInitialized))
    );
    assert_eq!(
        client.try_batch_pay_premiums(&caller, &ids),
        Err(Ok(InsuranceError::NotInitialized))
    );
    assert_eq!(
        client.try_deactivate_policy(&caller, &1),
        Err(Ok(InsuranceError::NotInitialized))
    );
    assert_eq!(
        client.try_set_external_ref(&caller, &1, &Some(external_ref(&env))),
        Err(Ok(InsuranceError::NotInitialized))
    );
}

/// Read-only entrypoints stay deterministic before init and never panic.
#[test]
fn test_reads_are_safe_before_init() {
    let (env, contract_id) = fresh_env();
    let client = InsuranceClient::new(&env, &contract_id);
    let owner = Address::generate(&env);

    assert!(client.get_policy(&1).is_none());
    assert_eq!(client.get_total_monthly_premium(&owner), 0);

    let page = client.get_active_policies(&owner, &0, &5);
    assert_eq!(page.items, Vec::new(&env));
    assert_eq!(page.next_cursor, 0);
    assert_eq!(page.count, 0);
}

/// The initialized owner remains the only admin for owner-only mutators.
#[test]
fn test_initialized_owner_is_only_privileged_owner() {
    let (env, contract_id) = fresh_env();
    let client = InsuranceClient::new(&env, &contract_id);
    let owner = init_contract(&client, &env);
    let policy_holder = Address::generate(&env);
    let stranger = Address::generate(&env);
    let policy_id = client.create_policy(
        &policy_holder,
        &policy_name(&env),
        &CoverageType::Health,
        &100,
        &10_000,
    );

    assert_eq!(
        client.set_external_ref(&owner, &policy_id, &Some(external_ref(&env))),
        true
    );
    assert_eq!(
        client.try_set_external_ref(&stranger, &policy_id, &None),
        Err(Ok(InsuranceError::Unauthorized))
    );

    assert_eq!(client.deactivate_policy(&owner, &policy_id), true);
    assert_eq!(
        client.try_deactivate_policy(&stranger, &policy_id),
        Err(Ok(InsuranceError::Unauthorized))
    );
}

/// `init` currently records no authorization requirement; ownership is
/// established purely by the first successful call.
#[test]
fn test_init_authorization_matches_current_model() {
    let (env, contract_id) = fresh_env();
    let client = InsuranceClient::new(&env, &contract_id);
    let owner = Address::generate(&env);

    client.init(&owner);
    assert!(
        env.auths().is_empty(),
        "init currently does not call require_auth on the proposed owner"
    );
}

/// Once initialized, owner-only and owner-scoped actions still enforce the
/// configured post-bootstrap authorization model.
#[test]
fn test_post_init_authorization_enforced() {
    let (env, contract_id) = fresh_env();
    let client = InsuranceClient::new(&env, &contract_id);
    let owner = init_contract(&client, &env);
    let policy_holder = Address::generate(&env);
    let other_user = Address::generate(&env);
    let policy_id = client.create_policy(
        &policy_holder,
        &policy_name(&env),
        &CoverageType::Health,
        &100,
        &10_000,
    );

    assert_eq!(
        client.try_pay_premium(&other_user, &policy_id),
        Err(Ok(InsuranceError::Unauthorized))
    );
    assert_eq!(
        client.try_set_external_ref(&other_user, &policy_id, &Some(external_ref(&env))),
        Err(Ok(InsuranceError::Unauthorized))
    );
    assert_eq!(
        client.set_external_ref(&owner, &policy_id, &Some(external_ref(&env))),
        true
    );
}
