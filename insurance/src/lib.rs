#![no_std]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use remitwise_common::{CoverageType, EventCategory, EventPriority, RemitwiseEvents};
use soroban_sdk::{
    contract, contractimpl, contracterror, contracttype, symbol_short, Address, Env, Map, String,
    Symbol, Vec,
};


// Storage TTL constants
const INSTANCE_LIFETIME_THRESHOLD: u32 = 17_280; // ~1 day
const INSTANCE_BUMP_AMOUNT: u32 = 518_400; // ~30 days

// Pagination constants
pub const DEFAULT_PAGE_LIMIT: u32 = 20;
pub const MAX_PAGE_LIMIT: u32 = 50;
const PAYMENT_PERIOD_SECONDS: u64 = 30 * 86_400;

/// Maximum number of active policies a single owner may hold.
/// When this cap is reached, `create_policy` returns `PolicyLimitExceeded`.
pub const MAX_POLICIES_PER_OWNER: u32 = 50;
/// Maximum monthly premium allowed for a single policy.
/// This bound ensures that 50 active policies cannot overflow `get_total_monthly_premium`.
pub const MAX_MONTHLY_PREMIUM: i128 = i128::MAX / MAX_POLICIES_PER_OWNER as i128;
/// Maximum coverage amount allowed for a single policy.
pub const MAX_COVERAGE_AMOUNT: i128 = i128::MAX / MAX_POLICIES_PER_OWNER as i128;

/// Maximum length for external reference strings
const MAX_EXTERNAL_REF_LEN: u32 = 128;

// Storage keys
const KEY_PAUSE_ADMIN: Symbol = symbol_short!("PAUSE_ADM");
const KEY_NEXT_ID: Symbol = symbol_short!("NEXT_ID");
const KEY_POLICIES: Symbol = symbol_short!("POLICIES");
/// `KEY_OWNER_INDEX` (OWN_IDX) maps each owner to a vector of policy IDs they own (active and inactive).
const KEY_OWNER_INDEX: Symbol = symbol_short!("OWN_IDX");
/// Instance-storage key for the external-reference index.
/// Holds a `Map<(Address, String), u32>` mapping each active `(owner, external_ref)` pair to its policy ID.
const KEY_EXT_REF_IDX: Symbol = symbol_short!("EXT_IDX");

// Event topic constants
/// Event topic symbol emitted by `set_external_ref` on every successful ref change. Payload is `ExternalRefUpdatedEvent`.
const EVT_EXT_REF_UPDATED: Symbol = symbol_short!("ext_upd");
const KEY_ARCHIVED: Symbol = symbol_short!("ARCH_POL");
const KEY_STATS: Symbol = symbol_short!("STOR_STAT");
const KEY_OWNER_ACTIVE: Symbol = symbol_short!("OWN_ACT");

/// Errors returned by the Insurance contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum InsuranceError {
    PolicyNotFound = 1,
    Unauthorized = 2,
    PolicyLimitExceeded = 3,
    PolicyInactive = 4,
    InvalidExternalRef = 5,
    DuplicateExternalRef = 6,
    MonthlyPremiumTooLow = 7,
    CoverageAmountTooLow = 8,
    MonthlyPremiumTooHigh = 9,
    CoverageAmountTooHigh = 10,
}

pub const EVT_POLICY_CREATED: Symbol = symbol_short!("created");
pub const EVT_PREMIUM_PAID: Symbol = symbol_short!("paid");
pub const EVT_POLICY_DEACTIVATED: Symbol = symbol_short!("deactive");

#[derive(Clone)]
#[contracttype]
pub struct PolicyCreatedEvent {
    pub policy_id: u32,
    pub owner: Address,
    pub coverage_type: CoverageType,
    pub monthly_premium: i128,
    pub coverage_amount: i128,
    pub timestamp: u64,
}

#[derive(Clone)]
#[contracttype]
pub struct PremiumPaidEvent {
    pub policy_id: u32,
    pub owner: Address,
    pub amount: i128,
    pub next_payment_date: u64,
    pub timestamp: u64,
}

#[derive(Clone)]
#[contracttype]
pub struct PolicyDeactivatedEvent {
    pub policy_id: u32,
    pub owner: Address,
    pub timestamp: u64,
}

/// Event emitted by `set_external_ref` on every successful external-reference change.
/// Carries the old and new ref values for off-chain indexers.
#[derive(Clone)]
#[contracttype]
pub struct ExternalRefUpdatedEvent {
    pub policy_id: u32,
    pub owner: Address,
    pub old_external_ref: Option<String>,
    pub new_external_ref: Option<String>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct InsurancePolicy {
    pub id: u32,
    pub owner: Address,
    pub name: String,
    pub external_ref: Option<String>,
    pub coverage_type: CoverageType,
    pub monthly_premium: i128,
    pub coverage_amount: i128,
    pub active: bool,
    pub next_payment_date: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct ArchivedPolicy {
    pub id: u32,
    pub owner: Address,
    pub name: String,
    pub external_ref: Option<String>,
    pub coverage_type: CoverageType,
    pub monthly_premium: i128,
    pub coverage_amount: i128,
    pub archived_at: u64,
    pub next_payment_date: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct PolicyPage {
    /// Active policies returned for this page.
    pub items: Vec<InsurancePolicy>,
    /// Cursor to resume from on the next call. `0` means end-of-list.
    pub next_cursor: u32,
    /// Number of items returned in `items`.
    pub count: u32,
}

#[contracttype]
#[derive(Clone)]
pub struct StorageStats {
    pub active_policies: u32,
    pub archived_policies: u32,
    pub last_updated: u64,
}

#[contract]
pub struct Insurance;

#[contractimpl]
impl Insurance {
    fn extend_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    }

    fn clamp_limit(limit: u32) -> u32 {
        if limit == 0 {
            DEFAULT_PAGE_LIMIT
        } else if limit > MAX_PAGE_LIMIT {
            MAX_PAGE_LIMIT
        } else {
            limit
        }
    }

    /// Validates that `ext_ref` is between 1 and 128 bytes (inclusive).
    /// Returns `Err(InsuranceError::InvalidExternalRef)` if the length is 0 or > 128.
    fn validate_external_ref(ext_ref: &String) -> Result<(), InsuranceError> {
        let len = ext_ref.len();
        if len == 0 || len > MAX_EXTERNAL_REF_LEN as usize {
            return Err(InsuranceError::InvalidExternalRef);
        }
        Ok(())
    }

    /// Reads `KEY_EXT_REF_IDX` from instance storage and returns the policy ID
    /// mapped to `ext_ref`, or `None` if no mapping exists.
    fn ext_idx_get(env: &Env, owner: &Address, ext_ref: &String) -> Option<u32> {
        let idx: Map<(Address, String), u32> = env
            .storage()
            .instance()
            .get(&KEY_EXT_REF_IDX)
            .unwrap_or_else(|| Map::new(env));
        idx.get((owner.clone(), ext_ref.clone()))
    }

    /// Loads `KEY_EXT_REF_IDX` (or creates a new empty map), inserts the
    /// `((owner, ext_ref) → policy_id)` mapping, and saves it back to instance storage.
    fn ext_idx_insert(env: &Env, owner: &Address, ext_ref: &String, policy_id: u32) {
        let mut idx: Map<(Address, String), u32> = env
            .storage()
            .instance()
            .get(&KEY_EXT_REF_IDX)
            .unwrap_or_else(|| Map::new(env));
        idx.set((owner.clone(), ext_ref.clone()), policy_id);
        env.storage().instance().set(&KEY_EXT_REF_IDX, &idx);
    }

    /// Loads `KEY_EXT_REF_IDX` (or creates a new empty map), removes the entry
    /// for `(owner, ext_ref)`, and saves it back to instance storage.
    fn ext_idx_remove(env: &Env, owner: &Address, ext_ref: &String) {
        let mut idx: Map<(Address, String), u32> = env
            .storage()
            .instance()
            .get(&KEY_EXT_REF_IDX)
            .unwrap_or_else(|| Map::new(env));
        idx.remove((owner.clone(), ext_ref.clone()));
        env.storage().instance().set(&KEY_EXT_REF_IDX, &idx);
    }

    fn read_stats(env: &Env) -> StorageStats {
        env.storage()
            .instance()
            .get(&KEY_STATS)
            .unwrap_or(StorageStats {
                active_policies: 0,
                archived_policies: 0,
                last_updated: 0,
            })
    }

    fn write_stats(env: &Env, stats: StorageStats) {
        env.storage().instance().set(&KEY_STATS, &stats);
    }

    fn owner_active_count(env: &Env, owner: &Address) -> u32 {
        let counts: Map<Address, u32> = env
            .storage()
            .instance()
            .get(&KEY_OWNER_ACTIVE)
            .unwrap_or_else(|| Map::new(env));
        counts.get(owner.clone()).unwrap_or(0)
    }

    fn adjust_owner_active(env: &Env, owner: &Address, delta: i32) {
        let mut counts: Map<Address, u32> = env
            .storage()
            .instance()
            .get(&KEY_OWNER_ACTIVE)
            .unwrap_or_else(|| Map::new(env));
        let current = counts.get(owner.clone()).unwrap_or(0);
        let next = if delta >= 0 {
            current.saturating_add(delta as u32)
        } else {
            current.saturating_sub((-delta) as u32)
        };
        counts.set(owner.clone(), next);
        env.storage().instance().set(&KEY_OWNER_ACTIVE, &counts);
    }

    fn get_external_ref_index(env: &Env) -> Map<String, u32> {
        env.storage()
            .instance()
            .get(&KEY_EXT_REF_IDX)
            .unwrap_or_else(|| Map::new(env))
    }

    fn bind_external_ref(env: &Env, owner: &Address, policy_id: u32, ext_ref: &Option<String>) {
        if let Some(r) = ext_ref {
            let mut index = Self::get_external_ref_index(env);
            if index.contains_key((owner.clone(), r.clone())) {
                panic!("external_ref already in use for owner");
            }
            index.set((owner.clone(), r.clone()), policy_id);
            env.storage().instance().set(&KEY_EXT_REF_IDX, &index);
        }
    }

    fn unbind_external_ref(env: &Env, owner: &Address, _policy_id: u32, ext_ref: &Option<String>) {
        if let Some(r) = ext_ref {
            let mut index = Self::get_external_ref_index(env);
            index.remove((owner.clone(), r.clone()));
            env.storage().instance().set(&KEY_EXT_REF_IDX, &index);
        }
    }

    pub fn set_pause_admin(env: Env, caller: Address, new_admin: Address) -> bool {
        caller.require_auth();
        Self::extend_instance_ttl(&env);
        env.storage().instance().set(&KEY_PAUSE_ADMIN, &new_admin);
        true
    }

    /// Creates a new insurance policy for the owner.
    ///
    /// # Active-count enforcement
    /// Checks `KEY_OWNER_ACTIVE` (OWN_ACT) to ensure the owner's active-policy count
    /// is strictly less than `MAX_POLICIES_PER_OWNER`. If at or above cap, returns `Err(PolicyLimitExceeded)`.
    ///
    /// # Index updates
    /// - Increments `KEY_OWNER_ACTIVE[owner]` by 1
    /// - Appends policy ID to `KEY_OWNER_INDEX[owner]`
    /// - If `external_ref` is `Some`, inserts into `KEY_EXT_REF_IDX[external_ref] = policy_id`
    ///
    /// # Errors
    /// - `InsuranceError::InvalidExternalRef` — if `external_ref` is `Some` but empty or longer than 128 bytes.
    /// - `InsuranceError::DuplicateExternalRef` — if `external_ref` is `Some` and already held by an active policy.
    /// - `InsuranceError::MonthlyPremiumTooLow` — if `monthly_premium <= 0`.
    /// - `InsuranceError::MonthlyPremiumTooHigh` — if `monthly_premium > MAX_MONTHLY_PREMIUM`.
    /// - `InsuranceError::CoverageAmountTooLow` — if `coverage_amount <= 0`.
    /// - `InsuranceError::CoverageAmountTooHigh` — if `coverage_amount > MAX_COVERAGE_AMOUNT`.
    pub fn create_policy(
        env: Env,
        owner: Address,
        name: String,
        coverage_type: CoverageType,
        monthly_premium: i128,
        coverage_amount: i128,
        external_ref: Option<String>,
    ) -> Result<u32, InsuranceError> {
        owner.require_auth();
        Self::extend_instance_ttl(&env);

        if monthly_premium <= 0 {
            return Err(InsuranceError::MonthlyPremiumTooLow);
        }
        if monthly_premium > MAX_MONTHLY_PREMIUM {
            return Err(InsuranceError::MonthlyPremiumTooHigh);
        }
        if coverage_amount <= 0 {
            return Err(InsuranceError::CoverageAmountTooLow);
        }
        if coverage_amount > MAX_COVERAGE_AMOUNT {
            return Err(InsuranceError::CoverageAmountTooHigh);
        }

        let active_count = Self::owner_active_count(&env, &owner);
        if active_count >= MAX_POLICIES_PER_OWNER {
            return Err(InsuranceError::PolicyLimitExceeded);
        }

        let mut next_id: u32 = env.storage().instance().get(&KEY_NEXT_ID).unwrap_or(0);
        next_id += 1;

        if let Some(ref r) = external_ref {
            Self::validate_external_ref(r)?;
        }

        // **Enforce active-count cap**: read OWN_ACT and check against MAX_POLICIES_PER_OWNER
        let active_count = Self::owner_active_count(&env, &owner);
        if active_count >= MAX_POLICIES_PER_OWNER {
            return Err(InsuranceError::PolicyLimitExceeded);
        }

        // Check for duplicate external_ref
        if let Some(ref r) = external_ref {
            if Self::ext_idx_get(&env, &owner, r).is_some() {
                return Err(InsuranceError::DuplicateExternalRef);
            }
        }

        // Allocate new policy ID
        let mut next_id: u32 = env.storage().instance().get(&KEY_NEXT_ID).unwrap_or(0);
        next_id += 1;

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));

        let policy = InsurancePolicy {
            id: next_id,
            owner: owner.clone(),
            name,
            external_ref: external_ref.clone(),
            coverage_type,
            monthly_premium,
            coverage_amount,
            active: true,
            next_payment_date: env
                .ledger()
                .timestamp()
                .saturating_add(PAYMENT_PERIOD_SECONDS),
        };

        // Store policy
        policies.set(next_id, policy);
        env.storage().instance().set(&KEY_POLICIES, &policies);

        // Update OWN_IDX: append to owner's policy list
        let mut index: Map<Address, Vec<u32>> = env
            .storage()
            .instance()
            .get(&KEY_OWNER_INDEX)
            .unwrap_or_else(|| Map::new(&env));
        let mut ids = index.get(owner.clone()).unwrap_or_else(|| Vec::new(&env));
        ids.push_back(next_id);
        index.set(owner.clone(), ids);
        env.storage().instance().set(&KEY_OWNER_INDEX, &index);

        // Update EXT_IDX if external_ref provided
        if let Some(ref r) = external_ref {
            Self::ext_idx_insert(&env, &owner, r, next_id);
        }

        // Persist next_id
        env.storage().instance().set(&KEY_NEXT_ID, &next_id);

        // **Increment OWN_ACT**: active count now increases by 1
        Self::adjust_owner_active(&env, &owner, 1);

        // Update storage stats
        let mut stats = Self::read_stats(&env);
        stats.active_policies += 1;
        stats.last_updated = env.ledger().timestamp();
        Self::write_stats(&env, stats);

        // Emit event
        RemitwiseEvents::emit(
            &env,
            EventCategory::Transaction,
            EventPriority::Medium,
            EVT_POLICY_CREATED,
            PolicyCreatedEvent {
                policy_id: next_id,
                owner,
                coverage_type,
                monthly_premium,
                coverage_amount,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(next_id)
    }

    pub fn get_policy(env: Env, policy_id: u32) -> Option<InsurancePolicy> {
        Self::extend_instance_ttl(&env);
        let policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));
        policies.get(policy_id)
    }

    /// Atomically updates a policy's `external_ref` and re-indexes `EXT_IDX`.
    ///
    /// - Removes the old `external_ref` from `EXT_IDX` (if `Some`).
    /// - Inserts the new `external_ref` into `EXT_IDX` (if `Some`).
    /// - If `new_ref` equals the current `external_ref`, returns `Ok(true)` immediately
    ///   without modifying storage or emitting an event (idempotent).
    /// - Emits `ExternalRefUpdatedEvent` (topic `EVT_EXT_REF_UPDATED`) on every successful change.
    ///
    /// # Errors
    /// - `InsuranceError::PolicyNotFound` — policy does not exist.
    /// - `InsuranceError::Unauthorized` — caller is not the policy owner.
    /// - `InsuranceError::PolicyInactive` — policy is not active.
    /// - `InsuranceError::InvalidExternalRef` — `new_ref` is `Some` but empty or > 128 bytes.
    /// - `InsuranceError::DuplicateExternalRef` — `new_ref` is already held by another active policy.
    pub fn try_set_external_ref(
        env: Env,
        caller: Address,
        policy_id: u32,
        new_ref: Option<String>,
    ) -> Result<bool, InsuranceError> {
        caller.require_auth();
        Self::extend_instance_ttl(&env);

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));

        let mut policy = match policies.get(policy_id) {
            Some(p) => p,
            None => return Err(InsuranceError::PolicyNotFound),
        };

        if policy.owner != caller {
            return Err(InsuranceError::Unauthorized);
        }

        if !policy.active {
            return Err(InsuranceError::PolicyInactive);
        }

        // Idempotent: if new_ref equals current ref, return immediately
        if new_ref == policy.external_ref {
            return Ok(true);
        }

        // Validate new ref length
        if let Some(ref r) = new_ref {
            Self::validate_external_ref(r)?;
        }

        // Duplicate check: skip the current policy's own entry
        if let Some(ref r) = new_ref {
            if let Some(existing_id) = Self::ext_idx_get(&env, &policy.owner, r) {
                if existing_id != policy_id {
                    return Err(InsuranceError::DuplicateExternalRef);
                }
            }
        }

        let old_ref = policy.external_ref.clone();

        // Remove old entry from index
        if let Some(ref r) = old_ref {
            Self::ext_idx_remove(&env, &policy.owner, r);
        }

        // Insert new entry into index
        if let Some(ref r) = new_ref {
            Self::ext_idx_insert(&env, &policy.owner, r, policy_id);
        }

        // Update policy record
        policy.external_ref = new_ref.clone();
        policies.set(policy_id, policy);
        env.storage().instance().set(&KEY_POLICIES, &policies);

        // Emit event
        let event = ExternalRefUpdatedEvent {
            policy_id,
            owner: caller.clone(),
            old_external_ref: old_ref,
            new_external_ref: new_ref,
            timestamp: env.ledger().timestamp(),
        };
        env.events().publish((EVT_EXT_REF_UPDATED,), event);

        Ok(true)
    }

    /// Deactivates a policy without removing it from storage.
    /// Sets `active = false` and removes its `external_ref` from `EXT_IDX`.
    /// Decrements the `OWN_ACT` active count and updates stats.
    /// Returns `Ok(false)` if the policy does not exist or the caller is not the owner.
    /// Idempotent: deactivating an already-inactive policy returns `Ok(true)` without decrementing again.
    pub fn deactivate_policy(env: Env, caller: Address, policy_id: u32) -> Result<bool, InsuranceError> {
        caller.require_auth();
        Self::extend_instance_ttl(&env);

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));
        
        let mut policy = match policies.get(policy_id) {
            Some(p) => p,
            None => return Ok(false),
        };
        
        if policy.owner != caller {
            return Ok(false);
        }
        policy.active = false;
        policies.set(policy_id, policy.clone());
        env.storage().instance().set(&KEY_POLICIES, &policies);
        if let Some(ref r) = policy.external_ref {
            Self::ext_idx_remove(&env, &policy.owner, r);
        }
        Ok(true)
    }

    /// Permanently removes a policy from active service and frees its `external_ref` for reuse.
    /// Removes the policy from `KEY_POLICIES` and removes its `external_ref` from `EXT_IDX`.
    /// Returns `Ok(false)` if the policy does not exist. Returns `Err(InsuranceError::Unauthorized)` if the caller is not the owner.
    pub fn try_archive_policy(env: Env, caller: Address, policy_id: u32) -> Result<bool, InsuranceError> {
        caller.require_auth();
        Self::extend_instance_ttl(&env);

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));

        let policy = match policies.get(policy_id) {
            Some(p) => p,
            None => return Ok(false),
        };

        if policy.owner != caller {
            return Err(InsuranceError::Unauthorized);
        }

        if let Some(ref r) = policy.external_ref {
            Self::ext_idx_remove(&env, &policy.owner, r);
        }

        // If active, update active counts and stats
        if policy.active {
            Self::unbind_external_ref(&env, &caller, policy_id, &policy.external_ref);
            Self::adjust_owner_active(&env, &caller, -1);

            // Update stats
            let mut stats = Self::read_stats(&env);
            stats.active_policies = stats.active_policies.saturating_sub(1);
            stats.last_updated = env.ledger().timestamp();
            Self::write_stats(&env, stats);

            // Emit event
            RemitwiseEvents::emit(
                &env,
                EventCategory::State,
                EventPriority::Medium,
                EVT_POLICY_DEACTIVATED,
                PolicyDeactivatedEvent {
                    policy_id,
                    owner: caller.clone(),
                    timestamp: env.ledger().timestamp(),
                },
            );
        }

        // Move policy to archived storage
        let mut archived: Map<u32, ArchivedPolicy> = env
            .storage()
            .instance()
            .get(&KEY_ARCHIVED)
            .unwrap_or_else(|| Map::new(&env));

        archived.set(
            policy_id,
            ArchivedPolicy {
                id: policy.id,
                owner: policy.owner.clone(),
                name: policy.name.clone(),
                external_ref: policy.external_ref.clone(),
                coverage_type: policy.coverage_type,
                monthly_premium: policy.monthly_premium,
                coverage_amount: policy.coverage_amount,
                archived_at: env.ledger().timestamp(),
                next_payment_date: policy.next_payment_date,
            },
        );

        // Remove from active policies
        policies.remove(policy_id);
        env.storage().instance().set(&KEY_POLICIES, &policies);
        env.storage().instance().set(&KEY_ARCHIVED, &archived);

        // Update archive count
        let mut stats = Self::read_stats(&env);
        stats.archived_policies = stats.archived_policies.saturating_add(1);
        stats.last_updated = env.ledger().timestamp();
        Self::write_stats(&env, stats);

        Ok(true)
    }

    pub fn set_external_ref(
        env: Env,
        caller: Address,
        policy_id: u32,
        external_ref: Option<String>,
    ) -> bool {
        match Self::try_set_external_ref(env, caller, policy_id, external_ref) {
            Ok(v) => v,
            Err(_) => false,
        }
    }

    pub fn archive_policy(env: Env, caller: Address, policy_id: u32) -> bool {
        match Self::try_archive_policy(env, caller, policy_id) {
            Ok(v) => v,
            Err(_) => false,
        }
    }

    pub fn restore_policy(env: Env, caller: Address, policy_id: u32) -> bool {
        caller.require_auth();
        Self::extend_instance_ttl(&env);

        let mut archived: Map<u32, ArchivedPolicy> = env
            .storage()
            .instance()
            .get(&KEY_ARCHIVED)
            .unwrap_or_else(|| Map::new(&env));
        let record = match archived.get(policy_id) {
            Some(r) => r,
            None => return false,
        };
        if record.owner != caller {
            return false;
        }

        let active_count = Self::owner_active_count(&env, &caller);
        if active_count >= MAX_POLICIES_PER_OWNER {
            return false;
        }

        if let Some(ref r) = record.external_ref {
            let index = Self::get_external_ref_index(&env);
            if index.contains_key((caller.clone(), r.clone())) {
                return false;
            }
        }

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));

        Self::bind_external_ref(&env, &caller, policy_id, &record.external_ref);
        policies.set(
            policy_id,
            InsurancePolicy {
                id: record.id,
                owner: record.owner,
                name: record.name,
                external_ref: record.external_ref,
                coverage_type: record.coverage_type,
                monthly_premium: record.monthly_premium,
                coverage_amount: record.coverage_amount,
                active: true,
                next_payment_date: record.next_payment_date,
            },
        );
        archived.remove(policy_id);

        env.storage().instance().set(&KEY_POLICIES, &policies);
        env.storage().instance().set(&KEY_ARCHIVED, &archived);

        Self::adjust_owner_active(&env, &caller, 1);
        let mut stats = Self::read_stats(&env);
        stats.archived_policies = stats.archived_policies.saturating_sub(1);
        stats.active_policies += 1;
        stats.last_updated = env.ledger().timestamp();
        Self::write_stats(&env, stats);

        true
    }

    pub fn get_archived_policy(env: Env, policy_id: u32) -> Option<ArchivedPolicy> {
        Self::extend_instance_ttl(&env);
        let archived: Map<u32, ArchivedPolicy> = env
            .storage()
            .instance()
            .get(&KEY_ARCHIVED)
            .unwrap_or_else(|| Map::new(&env));
        archived.get(policy_id)
    }

    pub fn get_policy_id_by_external_ref(
        env: Env,
        owner: Address,
        external_ref: String,
    ) -> Option<u32> {
        Self::extend_instance_ttl(&env);
        let index = Self::get_external_ref_index(&env);
        index.get((owner, external_ref))
    }

    /// Pays one premium and advances `next_payment_date` by the fixed 30-day cadence.
    ///
    /// The resulting due date is always in the future and is mirrored in
    /// `PremiumPaidEvent.next_payment_date`.
    pub fn pay_premium(env: Env, caller: Address, policy_id: u32) -> bool {
        caller.require_auth();
        Self::extend_instance_ttl(&env);

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));
        let mut policy = match policies.get(policy_id) {
            Some(p) => p,
            None => return false,
        };
        if policy.owner != caller || !policy.active {
            return false;
        }

        let amount = policy.monthly_premium;
        let now = env.ledger().timestamp();
        policy.next_payment_date = Self::advance_next_payment_date(policy.next_payment_date, now);
        let next_payment_date = policy.next_payment_date;
        policies.set(policy_id, policy);
        env.storage().instance().set(&KEY_POLICIES, &policies);

        RemitwiseEvents::emit(
            &env,
            EventCategory::Transaction,
            EventPriority::Low,
            EVT_PREMIUM_PAID,
            PremiumPaidEvent {
                policy_id,
                owner: caller,
                amount,
                next_payment_date,
                timestamp: now,
            },
        );

        true
    }

    /// Pays premiums in batch and advances each policy's due date independently
    /// using that policy's own `next_payment_date` plus fixed 30-day cadence rules.
    pub fn batch_pay_premiums(env: Env, caller: Address, policy_ids: Vec<u32>) -> u32 {
        caller.require_auth();
        Self::extend_instance_ttl(&env);

        let mut policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));

        let mut count: u32 = 0;
        let now = env.ledger().timestamp();

        for id in policy_ids.iter() {
            if let Some(mut p) = policies.get(id) {
                if p.owner == caller && p.active {
                    let amount = p.monthly_premium;
                    let next_date = Self::advance_next_payment_date(p.next_payment_date, now);
                    p.next_payment_date = next_date;
                    policies.set(id, p);

                    RemitwiseEvents::emit(
                        &env,
                        EventCategory::Transaction,
                        EventPriority::Low,
                        EVT_PREMIUM_PAID,
                        PremiumPaidEvent {
                            policy_id: id,
                            owner: caller.clone(),
                            amount,
                            next_payment_date: next_date,
                            timestamp: now,
                        },
                    );
                    count += 1;
                }
            }
        }
        env.storage().instance().set(&KEY_POLICIES, &policies);
        count
    }

    pub fn get_total_monthly_premium(env: Env, owner: Address) -> i128 {
        Self::extend_instance_ttl(&env);

        let policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));
        let index: Map<Address, Vec<u32>> = env
            .storage()
            .instance()
            .get(&KEY_OWNER_INDEX)
            .unwrap_or_else(|| Map::new(&env));

        let ids = index.get(owner).unwrap_or_else(|| Vec::new(&env));
        let mut total: i128 = 0;
        for id in ids.iter() {
            if let Some(p) = policies.get(id) {
                if p.active {
                    total += p.monthly_premium;
                }
            }
        }
        total
    }

    /// Returns a stable, cursor-based page of active policies for an owner.
    pub fn get_active_policies(env: Env, owner: Address, cursor: u32, limit: u32) -> PolicyPage {
        Self::extend_instance_ttl(&env);
        let limit = Self::clamp_limit(limit);

        let policies: Map<u32, InsurancePolicy> = env
            .storage()
            .instance()
            .get(&KEY_POLICIES)
            .unwrap_or_else(|| Map::new(&env));
        let index: Map<Address, Vec<u32>> = env
            .storage()
            .instance()
            .get(&KEY_OWNER_INDEX)
            .unwrap_or_else(|| Map::new(&env));
        let ids = index.get(owner).unwrap_or_else(|| Vec::new(&env));
        let sorted_ids = Self::sorted_unique_ids(&env, ids);

        let mut items: Vec<InsurancePolicy> = Vec::new(&env);
        let mut next_cursor: u32 = 0;
        let mut has_more = false;

        // Bounded read: iterate owner-indexed ids only (not the entire policy map).
        for id in sorted_ids.iter() {
            if id <= cursor {
                continue;
            }
            if let Some(p) = policies.get(id) {
                if p.active {
                    if items.len() < (limit as usize) {
                        items.push_back(p);
                        next_cursor = id;
                    } else {
                        has_more = true;
                        break;
                    }
                }
            }
        }

        let out_cursor = if has_more { next_cursor } else { 0 };
        PolicyPage {
            items,
            next_cursor: out_cursor,
            count: items.len() as u32,
        }
    }

    /// Helper: returns a deduplicated, sorted vector of policy IDs for the owner.
    fn sorted_unique_ids(env: &Env, ids: Vec<u32>) -> Vec<u32> {
        let mut sorted: Vec<u32> = Vec::new(env);
        let mut seen: Map<u32, bool> = Map::new(env);

        // Collect unique IDs
        for id in ids.iter() {
            if !seen.contains_key(id) {
                sorted.push_back(id);
                seen.set(id, true);
            }
        }

        // Simple bubble sort for small collections
        let len = sorted.len();
        for i in 0..len {
            for j in 0..(len - 1 - i) {
                if sorted.get(j).unwrap() > sorted.get(j + 1).unwrap() {
                    let temp = sorted.get(j).unwrap();
                    sorted.set(j, sorted.get(j + 1).unwrap());
                    sorted.set(j + 1, temp);
                }
            }
        }

        sorted
    }

    /// Helper: advances the next-payment date by one 30-day period (PAYMENT_PERIOD_SECONDS).
    /// If the current next_payment_date is in the past relative to `now`, returns a date
    /// that is at least PAYMENT_PERIOD_SECONDS into the future from `now`.
    fn advance_next_payment_date(current_next_date: u64, now: u64) -> u64 {
        let one_period_ahead = current_next_date.saturating_add(PAYMENT_PERIOD_SECONDS);
        if one_period_ahead > now {
            one_period_ahead
        } else {
            // If next_date is in the past, advance from now
            now.saturating_add(PAYMENT_PERIOD_SECONDS)
        }
    }

    pub fn get_storage_stats(env: Env) -> StorageStats {
        Self::extend_instance_ttl(&env);
        Self::read_stats(&env)
    }
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod next_payment_scheduling_tests;
