#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    BytesN, Env, String, Vec,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Desired lifetime for the ledger entries this contract keeps alive:
/// about a year, assuming Stellar's ~5s ledger close time.
///
/// 365 days * 24h * 60m * 60s / 5s-per-ledger = 6,307,200 ledgers.
///
/// This is only a *request*. The effective window is whatever the network
/// allows — `env.storage().max_ttl()`, read at call time — and every
/// `extend_ttl` site clamps to it. On testnet and mainnet today
/// `max_entry_ttl` is 3,110,400 ledgers (a network setting, changed by
/// validator vote), so a deployment record actually lives **about 180
/// days, not a year**. Each deployment refreshes the count key, but not
/// records written earlier, so a factory idle that long loses its index.
///
/// Deliberately not compared against a hardcoded ceiling: the previous
/// constant here (6,312,000) was the soroban-sdk *test harness* default
/// (`soroban-sdk/src/env.rs`), not a network value, so the clamp it fed
/// could never fire. The test environment still reports 6,312,000, which is
/// why no test in this file asserts a specific network figure.
const TTL_LEDGERS: u32 = 365 * 24 * 60 * 60 / 5;

/// Maximum lifetime for an admin transfer proposal before it becomes invalid.
///
/// This is a deadline in ledger numbers, not a storage TTL, so it is not
/// clamped to `max_ttl()`. Note the instance entry holding the proposal is
/// clamped, so on today's networks the entry's ~180-day window expires before
/// this ~365-day deadline does; a proposal left untouched that long needs the
/// instance kept alive by any other call. Every admin mutation below calls
/// `_bump_instance`, which is what keeps that entry alive.
const ADMIN_PROPOSAL_EXPIRY_LEDGERS: u32 = TTL_LEDGERS;

/// Minimum delay between proposing a token WASM hash and it becoming
/// acceptable to `accept_token_wasm_hash`.
///
/// Six hours of ledger time: 6h * 60m * 60s / 5s-per-ledger = 4,320
/// ledgers. Long enough that the `wasm_chg` event announcing a rotation is
/// seen — by a watcher on the other side of a timezone, by an indexer, by
/// anyone with the factory bookmarked — before the swap can complete, and
/// short enough that a legitimate token fix is not held up for days.
///
/// Like [`ADMIN_PROPOSAL_EXPIRY_LEDGERS`] this is a deadline in ledger
/// numbers, not a storage TTL, so it is not clamped to `max_ttl()`. The
/// pending proposal lives in instance storage, which
/// `propose_token_wasm_hash` refreshes with `_bump_instance`, so the entry
/// comfortably outlives the delay (today's networks clamp instance TTL to
/// ~180 days — far longer than six hours).
const TOKEN_WASM_CHANGE_DELAY_LEDGERS: u32 = 6 * 60 * 60 / 5;

/// Largest page a caller may request from `get_deployments_paginated`.
///
/// `limit` is caller-supplied, so with no ceiling a single read can ask the
/// host to build and serialise an arbitrarily large vector — burning the
/// invocation's compute budget and resource fee on a read that returns
/// nothing the caller did not already have (issue #469). Stellar's guidance
/// is that a function which reads storage in a loop must bound its
/// iterations, and that a caller-chosen page size is clamped in-contract
/// rather than trusted.
const MAX_PAGE: u32 = 100;

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// The address allowed to configure the factory (`propose_admin`,
    /// `propose_token_wasm_hash`). Removed by `revoke_admin`.
    Admin,
    /// The address proposed by `propose_admin`, waiting on `accept_admin`.
    PendingAdmin,
    /// Ledger at which `PendingAdmin` lapses. Written alongside
    /// `PendingAdmin`, cleared alongside it.
    PendingAdminExpiry,
    /// Set once on the first successful `initialize` and never removed.
    /// Unlike `Admin` (which `revoke_admin` deletes), this is the sole
    /// re-initialization guard, so revoking admin can never reopen
    /// `initialize`.
    Initialized,
    /// Set to `true` by `revoke_admin`. Once set, no admin operation can
    /// ever succeed again — the factory keeps deploying tokens against a
    /// frozen code hash, but nothing about its configuration can change.
    Locked,
    /// Set by `pause`, removed by `unpause`. While present, `deploy_token`
    /// and `accept_admin` are refused with [`FactoryError::Paused`].
    IsPaused,
    /// WASM hash of the token contract this factory deploys.
    TokenWasmHash,
    /// Replacement hash proposed by `propose_token_wasm_hash`, waiting on
    /// `accept_token_wasm_hash`. Removed by `accept_token_wasm_hash`,
    /// `cancel_token_wasm_proposal` and `revoke_admin`.
    PendingTokenWasmHash,
    /// Ledger from which `PendingTokenWasmHash` may be accepted. Written
    /// alongside the proposal, cleared alongside it.
    PendingTokenWasmEffective,
    /// Number of tokens deployed (used to derive `DeploymentAt` slots).
    DeploymentCount,
    /// Enumerated token address, index `0..DeploymentCount`.
    DeploymentAt(u32),
    /// Metadata registry this factory writes to in `deploy_token`.
    MetadataRegistry,
}

/// Complete configuration for the token `deploy_token` deploys.
///
/// Bundled into one parameter because Soroban caps a contract function at ten
/// host-visible arguments, and `deploy_token` needs eleven distinct values. A
/// single struct keeps the call atomic and fully specified while staying under
/// the limit.
#[derive(Clone)]
#[contracttype]
pub struct TokenConfig {
    pub admin: Address,
    pub decimal: u32,
    pub name: String,
    pub symbol: String,
    pub initial_supply: i128,
    pub max_supply: Option<i128>,
    pub authorization_required: bool,
    pub authorization_revocable: bool,
    pub compliance_node: Option<Address>,
    /// Canonical metadata JSON. The factory does not store it; it is hashed
    /// with keccak256 into the metadata registry in the same transaction as
    /// the deploy. `None` if the creator supplied nothing to commit to.
    pub metadata: Option<String>,
    /// Optional `https://` / `ipfs://` URI passed through to the token's
    /// `initialize` as `contract_uri` so the document location is on-chain
    /// in the same transaction. The JSON still lives off-chain; this is
    /// where to fetch it.
    pub contract_uri: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed contract errors — surfaced in release WASM as numeric codes.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum FactoryError {
    /// A config getter was called before `initialize`.
    NotInitialized = 1,
    /// `initialize` was called on a contract that is already initialized.
    AlreadyInitialized = 2,
    /// `deploy_token` was called before a token WASM hash was set.
    TokenWasmNotSet = 3,
    /// The contract is permanently locked (`revoke_admin` was called).
    Locked = 4,
    /// The contract is paused.
    Paused = 5,
    /// `accept_admin` was called with no pending proposal.
    NoPendingAdmin = 6,
    /// `accept_admin` was called after the proposal's expiry ledger.
    ProposalExpired = 7,
    /// WASM hash supplied to `upgrade` is the all-zeros sentinel.
    InvalidWasmHash = 8,
    /// `propose_admin` was called with the address that is already admin.
    InvalidAdmin = 9,
    /// `accept_token_wasm_hash` was called with no pending proposal.
    NoPendingWasmChange = 10,
    /// `accept_token_wasm_hash` was called before the proposal's delay
    /// ([`TOKEN_WASM_CHANGE_DELAY_LEDGERS`]) had elapsed.
    WasmChangeNotDue = 11,
    /// `deploy_token` was called before a metadata registry was configured.
    MetadataRegistryNotSet = 12,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// Atomic token deploy-and-initialize factory.
///
/// Sophisticated launchpads front-run a two-transaction token launch: a
/// competitor observes the `create_contract` (the deploy) in the mempool,
/// computes the deterministic token address, snapshots their own `initialize`
/// call against it first, and claims the `admin` role on the token the victim
/// was about to mint. `deploy_token` closes that window by deploying and
/// initialising the token in a single invocation — either both happen or
/// neither does.
#[contract]
pub struct FactoryContract;

#[contractimpl]
impl FactoryContract {
    // ── Administration ──────────────────────────────────────────────────

    /// Initialize the factory with the address allowed to configure it.
    ///
    /// `admin.require_auth()` is enforced so the caller must prove they
    /// control the admin address. Callable once.
    pub fn initialize(env: Env, admin: Address) {
        // Both keys are checked: `Initialized` is the guard going forward,
        // but a factory already initialized by an earlier build only has
        // `Admin`, and `revoke_admin` deletes `Admin` — so testing either
        // keeps a re-initialization path closed in both cases.
        if env.storage().instance().has(&DataKey::Initialized)
            || env.storage().instance().has(&DataKey::Admin)
        {
            panic_with_error!(&env, FactoryError::AlreadyInitialized);
        }
        admin.require_auth();

        let storage = env.storage().instance();
        storage.set(&DataKey::Initialized, &true);
        storage.set(&DataKey::Admin, &admin);
        let ttl = Self::_ttl_ledgers(&env);
        storage.extend_ttl(ttl, ttl);
        env.events().publish((symbol_short!("init"),), admin);
    }

    // ── Admin lifecycle ────────────────────────────────────────────────
    //
    // Deliberately the same surface as the token contract (see
    // docs/admin-capability-matrix.md): two-step transfer with an expiry,
    // revoke_admin, pause/unpause, upgrade, and an event for every one of
    // them.

    /// Propose a new admin. Must be called by the current admin.
    /// The new admin must call `accept_admin` to finalize the transfer.
    ///
    /// The proposal lapses [`ADMIN_PROPOSAL_EXPIRY_LEDGERS`] ledgers after it
    /// is made; `pending_admin()` stops reporting it from that ledger on.
    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::_require_admin(&env);
        if new_admin == current_admin {
            panic_with_error!(&env, FactoryError::InvalidAdmin);
        }

        let expiry_ledger = env
            .ledger()
            .sequence()
            .saturating_add(ADMIN_PROPOSAL_EXPIRY_LEDGERS);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdminExpiry, &expiry_ledger);

        Self::_bump_instance(&env);
        env.events()
            .publish((symbol_short!("prop_adm"), current_admin, new_admin), ());
    }

    /// Cancel a pending admin transfer. Must be called by the current admin.
    pub fn cancel_admin_proposal(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.storage()
            .instance()
            .remove(&DataKey::PendingAdminExpiry);

        Self::_bump_instance(&env);
        env.events().publish((symbol_short!("cncl_adm"),), ());
    }

    /// Accept the admin role. Must be called by the pending admin.
    pub fn accept_admin(env: Env) {
        Self::_require_not_locked(&env);
        Self::_check_paused(&env);

        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, FactoryError::NoPendingAdmin));
        let expiry_ledger: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdminExpiry)
            .unwrap_or(0);
        if env.ledger().sequence() >= expiry_ledger {
            panic_with_error!(&env, FactoryError::ProposalExpired);
        }

        pending.require_auth();
        let old_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, FactoryError::NotInitialized));
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.storage()
            .instance()
            .remove(&DataKey::PendingAdminExpiry);

        Self::_bump_instance(&env);
        env.events()
            .publish((symbol_short!("set_admin"), old_admin, pending), ());
    }

    /// Permanently revoke the admin role and lock the contract.
    ///
    /// After this call:
    /// - `propose_token_wasm_hash`, `accept_token_wasm_hash`,
    ///   `cancel_token_wasm_proposal`, `propose_admin`, `accept_admin`,
    ///   `pause`, `unpause` and `upgrade` can never succeed again.
    /// - The `Admin` storage entry is removed and a `Locked` flag is set.
    /// - Any pending token-WASM-hash proposal is dropped, so a rotation
    ///   announced before the revoke cannot be accepted after it.
    /// - `is_locked()` returns `true` from then on.
    ///
    /// `deploy_token` keeps working against whatever WASM hash is already
    /// recorded — revoking freezes the factory's configuration, it does not
    /// take the deployment service away from the people using it.
    ///
    /// **This action is irreversible.**
    pub fn revoke_admin(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().set(&DataKey::Locked, &true);
        env.storage().instance().remove(&DataKey::Admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.storage()
            .instance()
            .remove(&DataKey::PendingAdminExpiry);
        env.storage()
            .instance()
            .remove(&DataKey::PendingTokenWasmHash);
        env.storage()
            .instance()
            .remove(&DataKey::PendingTokenWasmEffective);

        Self::_bump_instance(&env);
        env.events().publish((symbol_short!("revoked"),), true);
    }

    /// Pause the factory. Admin only.
    ///
    /// While paused, `deploy_token` and `accept_admin` are refused with
    /// [`FactoryError::Paused`]. Read-only getters keep working so the
    /// deployment index stays inspectable.
    pub fn pause(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().set(&DataKey::IsPaused, &true);

        Self::_bump_instance(&env);
        env.events().publish((symbol_short!("pause"),), ());
    }

    /// Unpause the factory. Admin only.
    pub fn unpause(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().remove(&DataKey::IsPaused);

        Self::_bump_instance(&env);
        env.events().publish((symbol_short!("unpause"),), ());
    }

    /// Upgrade this contract's WASM code hash in place. Admin only.
    ///
    /// Security note: this preserves existing storage and contract address,
    /// so new WASM must remain storage-compatible with previous deployments.
    /// This is the factory's own code — the token hash `deploy_token` uses is
    /// changed separately with
    /// [`propose_token_wasm_hash`](Self::propose_token_wasm_hash) /
    /// [`accept_token_wasm_hash`](Self::accept_token_wasm_hash).
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::_require_admin(&env);
        if new_wasm_hash == BytesN::from_array(&env, &[0; 32]) {
            panic_with_error!(&env, FactoryError::InvalidWasmHash);
        }
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());

        Self::_bump_instance(&env);
        env.events()
            .publish((symbol_short!("upgrade"),), new_wasm_hash);
    }

    /// Return the configured admin address.
    pub fn get_admin(env: Env) -> Address {
        Self::_admin(&env)
    }

    /// Returns the address proposed via `propose_admin` that has not yet
    /// accepted the role, or `None` when no two-step transfer is in
    /// progress. The entry is written by `propose_admin` and cleared by
    /// `accept_admin`, `cancel_admin_proposal`, or `revoke_admin`; if the
    /// proposal has expired it is also cleared so stale state does not linger.
    pub fn pending_admin(env: Env) -> Option<Address> {
        let expiry_ledger: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdminExpiry)
            .unwrap_or(0);
        if env.ledger().sequence() >= expiry_ledger {
            env.storage().instance().remove(&DataKey::PendingAdmin);
            env.storage()
                .instance()
                .remove(&DataKey::PendingAdminExpiry);
            return None;
        }

        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Returns `true` once `revoke_admin` has been called. Once locked, no
    /// admin operation can ever succeed again.
    pub fn is_locked(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false)
    }

    /// Returns `true` if the factory is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsPaused)
            .unwrap_or(false)
    }

    // ── Token WASM hash ────────────────────────────────────────────────
    //
    // The hash `deploy_token` names is the factory's blast radius: every
    // launch after a rotation deploys whatever code it points at, to users
    // who recognise the factory's address but not the build behind it. So
    // it changes the way the admin does — proposed, delayed, accepted —
    // and the change is announced with both hashes in the event, before it
    // can take effect.

    /// Propose a new WASM hash for `deploy_token`. Must be called by the
    /// current admin.
    ///
    /// The currently recorded hash keeps deploying until
    /// [`accept_token_wasm_hash`](Self::accept_token_wasm_hash) runs at
    /// least [`TOKEN_WASM_CHANGE_DELAY_LEDGERS`] ledgers (six hours) later,
    /// so a rotation cannot land in the same block it was decided in.
    ///
    /// The `wasm_chg` event carries the hash in force (`current` — the
    /// all-zeros sentinel when the factory has never had one), the proposed
    /// replacement, and the ledger from which it becomes acceptable. A third
    /// party watching the factory therefore learns about every rotation
    /// while there is still time to object, up to and including
    /// [`cancel_token_wasm_proposal`](Self::cancel_token_wasm_proposal).
    ///
    /// The WASM must already be installed on the network — this only
    /// records the hash. Proposing again replaces any pending proposal and
    /// restarts the delay from the new proposal's ledger.
    pub fn propose_token_wasm_hash(env: Env, wasm_hash: BytesN<32>) {
        Self::_require_admin(&env);
        if wasm_hash == BytesN::from_array(&env, &[0; 32]) {
            panic_with_error!(&env, FactoryError::InvalidWasmHash);
        }

        // The hash still in force. All-zeros doubles as "unset": a real
        // hash can never be zeros, because the guard above rejects them.
        let current: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::TokenWasmHash)
            .unwrap_or_else(|| BytesN::from_array(&env, &[0; 32]));
        let effective_ledger = env
            .ledger()
            .sequence()
            .saturating_add(TOKEN_WASM_CHANGE_DELAY_LEDGERS);

        env.storage()
            .instance()
            .set(&DataKey::PendingTokenWasmHash, &wasm_hash);
        env.storage()
            .instance()
            .set(&DataKey::PendingTokenWasmEffective, &effective_ledger);

        Self::_bump_instance(&env);
        env.events().publish(
            (symbol_short!("wasm_chg"), current, wasm_hash),
            effective_ledger,
        );
    }

    /// Cancel a pending token WASM hash proposal. Must be called by the
    /// current admin. The factory keeps deploying the hash it already has.
    ///
    /// The lever the admin pulls when a `wasm_chg` watcher objects during
    /// the delay window — the proposal dies, and nothing was ever deployed
    /// from it.
    pub fn cancel_token_wasm_proposal(env: Env) {
        Self::_require_admin(&env);
        env.storage()
            .instance()
            .remove(&DataKey::PendingTokenWasmHash);
        env.storage()
            .instance()
            .remove(&DataKey::PendingTokenWasmEffective);

        Self::_bump_instance(&env);
        env.events().publish((symbol_short!("cncl_wasm"),), ());
    }

    /// Accept the pending token WASM hash. Must be called by the current
    /// admin, and not before the delay the proposal was made with has
    /// elapsed.
    ///
    /// From here on every `deploy_token` deploys the new hash; the
    /// `set_wasm` event marks the ledger at which the change took effect.
    pub fn accept_token_wasm_hash(env: Env) {
        Self::_require_admin(&env);

        let pending: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::PendingTokenWasmHash)
            .unwrap_or_else(|| panic_with_error!(&env, FactoryError::NoPendingWasmChange));
        let effective_ledger: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PendingTokenWasmEffective)
            .unwrap_or(0);
        if env.ledger().sequence() < effective_ledger {
            panic_with_error!(&env, FactoryError::WasmChangeNotDue);
        }

        env.storage()
            .instance()
            .set(&DataKey::TokenWasmHash, &pending);
        env.storage()
            .instance()
            .remove(&DataKey::PendingTokenWasmHash);
        env.storage()
            .instance()
            .remove(&DataKey::PendingTokenWasmEffective);

        Self::_bump_instance(&env);
        env.events().publish((symbol_short!("set_wasm"),), pending);
    }

    /// Return the configured token WASM hash.
    pub fn get_token_wasm_hash(env: Env) -> BytesN<32> {
        env.storage()
            .instance()
            .get(&DataKey::TokenWasmHash)
            .unwrap_or_else(|| panic_with_error!(&env, FactoryError::TokenWasmNotSet))
    }

    /// Returns the hash proposed via `propose_token_wasm_hash` together
    /// with the ledger from which it may be accepted, or `None` when no
    /// proposal is outstanding.
    ///
    /// Unlike `pending_admin` this proposal never lapses: it stays until it
    /// is accepted, cancelled, or replaced, so the `effective_ledger` half
    /// is what tells a caller whether it is due yet.
    pub fn pending_token_wasm_hash(env: Env) -> Option<(BytesN<32>, u32)> {
        let storage = env.storage().instance();
        storage
            .get(&DataKey::PendingTokenWasmHash)
            .zip(storage.get(&DataKey::PendingTokenWasmEffective))
    }

    /// Point this factory at a metadata registry. Admin only.
    ///
    /// Every subsequent `deploy_token` writes the launch's indexed record
    /// there in the same invocation. Replacing the address does not migrate
    /// existing records — those stay on the contract they were written to.
    pub fn set_metadata_registry(env: Env, registry: Address) {
        Self::_require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::MetadataRegistry, &registry);
        Self::_bump_instance(&env);
        env.events()
            .publish((symbol_short!("set_reg"),), registry);
    }

    /// Return the configured metadata registry, if any.
    pub fn get_metadata_registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::MetadataRegistry)
    }

    // ── Deployment ──────────────────────────────────────────────────────

    /// Deploy and initialize a token contract atomically.
    ///
    /// The token address is derived deterministically from `deployer` and
    /// `salt` (see [`get_deployment_address`](Self::get_deployment_address))
    /// and is returned so callers do not have to parse transaction meta.
    ///
    /// `deployer.require_auth()` binds the deployment to the deployer's
    /// account — nobody can deploy a token under someone else's salt. The
    /// token's own `initialize` additionally requires `admin.require_auth()`,
    /// which simulates to the same preimage as `deployer` when the frontend
    /// passes the deployer's public key as `admin`, so a single wallet
    /// signature covers both. If the token rejects any parameter or the
    /// initialization itself fails, the entire invocation — including the
    /// freshly deployed instance — is reverted.
    ///
    /// `metadata` (canonical JSON) is hashed into the metadata registry in
    /// this same invocation, and `contract_uri` is forwarded to the token's
    /// `initialize` so the document location is set atomically with the
    /// launch. A missing registry address is a hard error — a launch with
    /// no indexed record is how tokens become anonymous to third parties.
    ///
    /// Token parameters ride in a single [`TokenConfig`] struct so the
    /// function stays at three host-visible arguments (the SDK caps contract
    /// functions at ten).
    ///
    /// The WASM this deploys is whatever `accept_token_wasm_hash` last
    /// recorded — a rotation proposed but not yet accepted does not touch
    /// what is deployed here, which is the point of the delay.
    pub fn deploy_token(
        env: Env,
        deployer: Address,
        salt: BytesN<32>,
        config: TokenConfig,
    ) -> Address {
        Self::_check_paused(&env);
        deployer.require_auth();

        // Enforce that a token WASM hash is configured (error in
        // `test_deploy_token_requires_set_wasm_hash`). Only the live build
        // binds the hash — the test build derives the address instead.
        #[cfg(not(test))]
        let wasm_hash = Self::_require_token_wasm(&env);
        #[cfg(test)]
        let _ = Self::_require_token_wasm(&env);

        // Deploy and initialize in the same invocation. The token's own
        // validation (decimals, supply caps, auth) runs here; any panic
        // reverts the deployment too.
        //
        // The deterministic address is derived from `deployer` + `salt`
        // (see `get_deployment_address`). In live builds the contract is
        // actually installed at that address by `deploy`. Under `#[cfg(test)]`
        // the wasm cannot be uploaded (the test host's VM rejects the
        // reference-types the SDK-21 target always emits), so tests instead
        // pre-register a Rust token implementation at the same address via
        // `env.register_contract`; `deployed_address()` yields that same
        // address without touching the host VM.
        #[cfg(not(test))]
        let token_address = env
            .deployer()
            .with_address(deployer.clone(), salt.clone())
            .deploy(wasm_hash);

        #[cfg(test)]
        let token_address = env
            .deployer()
            .with_address(deployer.clone(), salt.clone())
            .deployed_address();

        Self::_finalize_deployment(&env, &token_address, deployer, salt, config);

        token_address
    }

    /// Return the deterministic address a `deploy_token` call with these
    /// `deployer`/`salt` values will produce, whether or not it has been
    /// deployed yet.
    pub fn get_deployment_address(env: Env, deployer: Address, salt: BytesN<32>) -> Address {
        env.deployer()
            .with_address(deployer, salt)
            .deployed_address()
    }

    // ── Deployment index ────────────────────────────────────────────────

    /// Return the number of tokens deployed through this factory.
    pub fn get_deployment_count(env: Env) -> u32 {
        Self::_deployment_count(&env)
    }

    /// Return a paginated list of deployed token addresses.
    ///
    /// `start` — zero-based offset into the deployment list.
    /// `limit` — maximum number of addresses to return, **clamped to
    /// `MAX_PAGE` (100)**. A larger request is served as a 100-entry page
    /// rather than rejected, so an over-eager client still makes progress
    /// instead of getting nothing back (#469).
    pub fn get_deployments_paginated(env: Env, start: u32, limit: u32) -> Vec<Address> {
        let total = Self::_deployment_count(&env);

        if start >= total {
            return Vec::new(&env);
        }

        let end = start.saturating_add(limit.min(MAX_PAGE)).min(total);

        let mut paginated = Vec::new(&env);
        let mut i = start;
        while i < end {
            if let Some(token) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::DeploymentAt(i))
            {
                paginated.push_back(token);
            }
            i += 1;
        }
        paginated
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Initialise the freshly deployed token, record it in the deployment
    /// index, and emit the `deploy` event. Kept as a single step so the
    /// deploy-then-initialize sequence stays atomic: if `initialize` rejects
    /// any parameter the whole invocation — including the record and event —
    /// reverts. Split out from `deploy_token` so tests can drive it against a
    /// token registered directly in the host (`env.register_contract`) without
    /// uploading WASM.
    fn _finalize_deployment(
        env: &Env,
        token_address: &Address,
        deployer: Address,
        salt: BytesN<32>,
        config: TokenConfig,
    ) {
        soroban_token::TokenContractClient::new(env, token_address).initialize(
            &config.admin,
            &config.decimal,
            &config.name,
            &config.symbol,
            &config.initial_supply,
            &config.max_supply,
            &config.authorization_required,
            &config.authorization_revocable,
            &config.compliance_node,
            &config.contract_uri,
        );

        Self::_register_metadata(env, token_address, &deployer, &config);
        Self::_record_deployment(env, token_address);

        env.events().publish(
            (symbol_short!("deploy"), deployer, salt),
            token_address.clone(),
        );
    }

    fn _admin(env: &Env) -> Address {
        Self::_require_not_locked(env);
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, FactoryError::NotInitialized))
    }

    /// Admin gate for every mutating admin operation: the contract must not
    /// be locked, and the caller must be the current admin.
    fn _require_admin(env: &Env) -> Address {
        let admin = Self::_admin(env);
        admin.require_auth();
        admin
    }

    fn _require_not_locked(env: &Env) {
        let locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false);
        if locked {
            panic_with_error!(env, FactoryError::Locked);
        }
    }

    /// Circuit breaker. Gates `deploy_token` plus `accept_admin`, so an
    /// in-flight admin transfer cannot complete while the contract is
    /// halted. Policy setters (`propose_token_wasm_hash`,
    /// `accept_token_wasm_hash`, `cancel_token_wasm_proposal`) and read-only
    /// getters stay available.
    fn _check_paused(env: &Env) {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::IsPaused)
            .unwrap_or(false)
        {
            panic_with_error!(env, FactoryError::Paused);
        }
    }

    /// Extend the instance entry's TTL so a proposal held in instance
    /// storage — an admin transfer, or a token-WASM-hash rotation — is not
    /// archived out from under itself.
    fn _bump_instance(env: &Env) {
        let ttl = Self::_ttl_ledgers(env);
        env.storage().instance().extend_ttl(ttl, ttl);
    }

    fn _require_token_wasm(env: &Env) -> BytesN<32> {
        env.storage()
            .instance()
            .get(&DataKey::TokenWasmHash)
            .unwrap_or_else(|| panic_with_error!(env, FactoryError::TokenWasmNotSet))
    }

    fn _deployment_count(env: &Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::DeploymentCount)
            .unwrap_or(0)
    }

    /// The TTL to request for a deployment record: our desired window, capped
    /// at what the network will actually honour.
    ///
    /// `soroban-env-host` silently lowers an over-long `extend_ttl` on a
    /// persistent entry rather than erroring, so an unclamped call is not a
    /// hard failure — it just quietly gets you a shorter entry than the code
    /// appears to ask for. Clamping here keeps the requested and effective
    /// values the same, so the archival window is legible from the source.
    fn _ttl_ledgers(env: &Env) -> u32 {
        TTL_LEDGERS.min(env.storage().max_ttl())
    }

    /// Write the launch into the metadata registry. Missing registry is a
    /// hard error so a successful `deploy_token` always leaves an indexed
    /// record a third party can read without SoroPad's API.
    fn _register_metadata(
        env: &Env,
        token_address: &Address,
        deployer: &Address,
        config: &TokenConfig,
    ) {
        let registry: Address = env
            .storage()
            .instance()
            .get(&DataKey::MetadataRegistry)
            .unwrap_or_else(|| panic_with_error!(env, FactoryError::MetadataRegistryNotSet));

        soroban_metadata_registry::MetadataRegistryContractClient::new(env, &registry)
            .register(
                token_address,
                &config.symbol,
                &config.name,
                deployer,
                &config.initial_supply,
                &config.metadata,
            );
    }

    fn _record_deployment(env: &Env, token: &Address) {
        let count = Self::_deployment_count(env);

        let key = DataKey::DeploymentAt(count);
        env.storage().persistent().set(&key, token);

        let count_key = DataKey::DeploymentCount;
        env.storage().persistent().set(&count_key, &(count + 1));

        let ttl_ledgers = Self::_ttl_ledgers(env);
        env.storage()
            .persistent()
            .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
        env.storage()
            .persistent()
            .extend_ttl(&count_key, ttl_ledgers, ttl_ledgers);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
        Env, IntoVal, Symbol, TryFromVal,
    };

    // ── Event topic fixture ─────────────────────────────────────────────
    //
    // The checked-in, single source of truth for every event topic-0 name
    // this contract emits. See the identical fixture in the token contract
    // (issue #340) — `scripts/generate_events_doc.py --check` re-derives this
    // set from the contract source to keep `docs/events.json` honest, but
    // keeping the same self-verifying fixture here catches drift without
    // running the script.
    const EXPECTED_TOPICS: [&str; 13] = [
        "init",
        "set_wasm",
        "wasm_chg",
        "cncl_wasm",
        "set_reg",
        "deploy",
        "prop_adm",
        "cncl_adm",
        "set_admin",
        "revoked",
        "pause",
        "unpause",
        "upgrade",
    ];

    /// Asserts the set of `symbol_short!("...")` topic-0 literals used in
    /// this file's production code exactly matches `EXPECTED_TOPICS` — the
    /// same static source-scan the token contract uses, because a live
    /// invocation test cannot exercise every topic.
    #[test]
    fn test_emitted_topics_match_checked_in_fixture() {
        const SOURCE: &str = include_str!("lib.rs");
        let (production_source, _) = SOURCE
            .split_once("#[cfg(test)]\nmod test {")
            .expect("could not locate test module boundary in lib.rs");

        const NEEDLE: &str = "symbol_short!(\"";

        for topic in EXPECTED_TOPICS {
            let mut rest = production_source;
            let mut found = false;
            while let Some(pos) = rest.find(NEEDLE) {
                let after = &rest[pos + NEEDLE.len()..];
                if after.len() > topic.len()
                    && after.starts_with(topic)
                    && after.as_bytes()[topic.len()] == b'"'
                {
                    found = true;
                    break;
                }
                rest = &after[1..];
            }
            assert!(
                found,
                "topic {topic:?} is listed in EXPECTED_TOPICS but no \
                 symbol_short!(\"{topic}\") literal was found in the contract"
            );
        }

        let mut rest = production_source;
        while let Some(pos) = rest.find(NEEDLE) {
            let after = &rest[pos + NEEDLE.len()..];
            let end = after.find('"').expect("unterminated symbol_short! literal");
            let name = &after[..end];
            assert!(
                EXPECTED_TOPICS.contains(&name),
                "topic {name:?} is emitted by the contract but missing from \
                 EXPECTED_TOPICS"
            );
            rest = &after[end..];
        }
    }

    // ── Shared setup ────────────────────────────────────────────────────

    fn setup(env: &Env) -> (Address, FactoryContractClient<'static>, Address) {
        let contract_id = env.register_contract(None, FactoryContract);
        let client = FactoryContractClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.initialize(&admin);
        (contract_id, client, admin)
    }

    /// A dummy token WASM hash. The test host cannot upload real SDK-21
    /// WASM, but the factory only records an opaque 32-byte hash, so any
    /// value works. Non-zero, because that is the one value the contract
    /// rejects outright.
    fn dummy_wasm_hash(env: &Env) -> BytesN<32> {
        BytesN::from_array(env, &[7u8; 32])
    }

    /// Drive the full two-step hash change — propose, sit out
    /// [`TOKEN_WASM_CHANGE_DELAY_LEDGERS`], accept — so tests that only need
    /// a configured factory do not each re-implement the delay.
    fn set_token_wasm(env: &Env, client: &FactoryContractClient<'static>, hash: &BytesN<32>) {
        client.propose_token_wasm_hash(hash);
        env.ledger()
            .set_sequence_number(env.ledger().sequence() + TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        client.accept_token_wasm_hash();
    }

    /// A `TokenConfig` with the values the rest of the tests assume (name
    /// "Factory Token", symbol "FTK", decimals 7, initial supply 1M).
    fn default_config(env: &Env, admin: &Address) -> TokenConfig {
        TokenConfig {
            admin: admin.clone(),
            decimal: 7,
            name: String::from_str(env, "Factory Token"),
            symbol: String::from_str(env, "FTK"),
            initial_supply: 1_000_000i128,
            max_supply: None,
            authorization_required: false,
            authorization_revocable: false,
            compliance_node: None,
            metadata: None,
            contract_uri: None,
        }
    }

    /// Register the *real* token contract (as Rust, not WASM) at the exact
    /// deterministic address the factory derives for `deployer`/`salt`, so the
    /// `cfg(test)` deploy path can drive its real `initialize` without the
    /// host uploading any WASM.
    fn register_token_at(env: &Env, deployer: &Address, salt: &BytesN<32>) -> Address {
        let address = env
            .deployer()
            .with_address(deployer.clone(), salt.clone())
            .deployed_address();
        env.register_contract(&Some(address.clone()), soroban_token::TokenContract);
        address
    }

    fn configured_factory(env: &Env) -> (FactoryContractClient<'static>, Address) {
        let (factory_id, client, admin) = setup(env);
        set_token_wasm(env, &client, &dummy_wasm_hash(env));
        attach_registry(env, &factory_id, &client, &admin);
        (client, admin)
    }

    fn attach_registry(
        env: &Env,
        factory_id: &Address,
        factory_client: &FactoryContractClient<'static>,
        admin: &Address,
    ) -> Address {
        let registry_id = env.register_contract(
            None,
            soroban_metadata_registry::MetadataRegistryContract,
        );
        let registry = soroban_metadata_registry::MetadataRegistryContractClient::new(
            env,
            &registry_id,
        );
        registry.initialize(admin, factory_id);
        factory_client.set_metadata_registry(&registry_id);
        registry_id
    }

    fn deploy_token(
        env: &Env,
        client: &FactoryContractClient<'static>,
        deployer: &Address,
        salt: &BytesN<32>,
        admin: &Address,
    ) -> Address {
        let config = default_config(env, admin);
        register_token_at(env, deployer, salt);
        client.deploy_token(deployer, salt, &config)
    }

    // Convenience helper that builds a `TokenConfig` with a custom decimal and
    // optional max supply, so failure-mode tests can override individual fields.
    // Returns `true` when the (outer) invocation result is an error.
    fn deploy_token_fails(
        env: &Env,
        client: &FactoryContractClient<'static>,
        deployer: &Address,
        salt: &BytesN<32>,
        admin: &Address,
        decimal: u32,
        max_supply: Option<i128>,
    ) -> bool {
        let config = TokenConfig {
            admin: admin.clone(),
            decimal,
            name: String::from_str(env, "T"),
            symbol: String::from_str(env, "T"),
            initial_supply: 1i128,
            max_supply,
            authorization_required: false,
            authorization_revocable: false,
            compliance_node: None,
            metadata: None,
            contract_uri: None,
        };
        register_token_at(env, deployer, salt);
        client.try_deploy_token(deployer, salt, &config).is_err()
    }

    // ── Administration ──────────────────────────────────────────────────

    #[test]
    fn test_initialize_sets_admin_and_is_callable_once() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);

        assert_eq!(client.get_admin(), admin);
        // Re-initialization is rejected.
        assert!(client.try_initialize(&admin).is_err());
    }

    // ── Token WASM hash rotation ────────────────────────────────────────
    //
    // `deploy_token` deploys whatever hash the admin last *accepted*, and
    // accepting is only possible after a delay that the `wasm_chg` event
    // announced to everyone. Both halves matter: the delay is what makes a
    // hostile rotation survivable, the event is what makes it visible.

    #[test]
    fn test_propose_and_accept_token_wasm_hash() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        let first = dummy_wasm_hash(&env);
        let second = BytesN::from_array(&env, &[9u8; 32]);

        // Configuring the factory the first time goes through both steps.
        client.propose_token_wasm_hash(&first);
        // A proposal is not a change: nothing is deployed from it, and the
        // hash is still unset...
        assert!(client.try_get_token_wasm_hash().is_err());
        assert_eq!(
            client.pending_token_wasm_hash(),
            Some((first.clone(), TOKEN_WASM_CHANGE_DELAY_LEDGERS))
        );

        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        client.accept_token_wasm_hash();
        assert_eq!(client.get_token_wasm_hash(), first);
        assert_eq!(client.pending_token_wasm_hash(), None);

        // Rotating: the hash in force keeps serving `deploy_token` for the
        // whole delay...
        client.propose_token_wasm_hash(&second);
        assert_eq!(client.get_token_wasm_hash(), first);
        assert_eq!(
            client.pending_token_wasm_hash(),
            Some((
                second.clone(),
                env.ledger().sequence() + TOKEN_WASM_CHANGE_DELAY_LEDGERS
            ))
        );

        // ...and only the accept swaps it.
        env.ledger()
            .set_sequence_number(2 * TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        client.accept_token_wasm_hash();
        assert_eq!(client.get_token_wasm_hash(), second);
        assert_eq!(client.pending_token_wasm_hash(), None);
    }

    #[test]
    fn test_propose_token_wasm_hash_requires_admin_auth() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (contract_id, client, _admin) = setup(&env);

        let user = Address::generate(&env);
        let wasm = dummy_wasm_hash(&env);

        // Only the user can auth — not the admin. `propose_token_wasm_hash`
        // must reject the call.
        env.mock_auths(&[MockAuth {
            address: &user,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "propose_token_wasm_hash",
                args: (wasm.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_propose_token_wasm_hash(&wasm).is_err());
        assert_eq!(client.pending_token_wasm_hash(), None);
    }

    #[test]
    fn test_accept_token_wasm_hash_requires_admin_auth() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (contract_id, client, _) = setup(&env);
        let wasm = dummy_wasm_hash(&env);

        client.propose_token_wasm_hash(&wasm);
        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);

        // Only the user can auth — not the admin. A failed accept leaves
        // the proposal pending and the recorded hash untouched.
        env.mock_auths(&[MockAuth {
            address: &Address::generate(&env),
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "accept_token_wasm_hash",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_accept_token_wasm_hash().is_err());
        assert_eq!(
            client.pending_token_wasm_hash(),
            Some((wasm.clone(), TOKEN_WASM_CHANGE_DELAY_LEDGERS))
        );
        assert!(client.try_get_token_wasm_hash().is_err());
    }

    #[test]
    fn test_accept_token_wasm_hash_rejects_before_the_delay() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        let wasm = dummy_wasm_hash(&env);

        client.propose_token_wasm_hash(&wasm);

        // One ledger short of the delay: refused, and still refused on the
        // last ledger before it — the delay is a floor, not a target.
        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS - 1);
        assert_eq!(
            client.try_accept_token_wasm_hash(),
            Err(Ok(FactoryError::WasmChangeNotDue.into()))
        );
        assert!(client.try_get_token_wasm_hash().is_err());

        // Exactly at the delay it goes through.
        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        client.accept_token_wasm_hash();
        assert_eq!(client.get_token_wasm_hash(), wasm);
    }

    #[test]
    fn test_accept_token_wasm_hash_without_proposal() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        assert_eq!(
            client.try_accept_token_wasm_hash(),
            Err(Ok(FactoryError::NoPendingWasmChange.into()))
        );
    }

    #[test]
    fn test_propose_token_wasm_hash_rejects_zero_hash() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        assert_eq!(
            client.try_propose_token_wasm_hash(&BytesN::from_array(&env, &[0u8; 32])),
            Err(Ok(FactoryError::InvalidWasmHash.into()))
        );
        assert_eq!(client.pending_token_wasm_hash(), None);
    }

    /// The event a third party watches: the hash in force, the proposed
    /// replacement, and the ledger from which it becomes acceptable. On a
    /// factory that has never been configured the "hash in force" is the
    /// all-zeros sentinel — a real hash can never be zeros.
    #[test]
    fn test_proposal_event_carries_current_proposed_and_effective_ledger() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        type Event = (
            soroban_sdk::Address,
            soroban_sdk::Vec<soroban_sdk::Val>,
            soroban_sdk::Val,
        );

        /// The single most recent event, wrapped in a `Vec` so it compares
        /// through `Vec`'s host-side `PartialEq` (`Val` itself has none).
        fn last_event(env: &Env) -> soroban_sdk::Vec<Event> {
            let events = env.events().all();
            events.slice(events.len() - 1..)
        }

        let first = dummy_wasm_hash(&env);
        client.propose_token_wasm_hash(&first);
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (
                        symbol_short!("wasm_chg"),
                        BytesN::from_array(&env, &[0u8; 32]),
                        first.clone()
                    )
                        .into_val(&env),
                    TOKEN_WASM_CHANGE_DELAY_LEDGERS.into_val(&env)
                )
            ]
        );

        // Once a hash is set, the next proposal names it as the one being
        // replaced, and counts the delay from its own ledger.
        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        client.accept_token_wasm_hash();

        let second = BytesN::from_array(&env, &[9u8; 32]);
        client.propose_token_wasm_hash(&second);
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("wasm_chg"), first, second).into_val(&env),
                    (2 * TOKEN_WASM_CHANGE_DELAY_LEDGERS).into_val(&env)
                )
            ]
        );
    }

    #[test]
    fn test_cancel_token_wasm_proposal() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        let wasm = dummy_wasm_hash(&env);

        client.propose_token_wasm_hash(&wasm);
        assert_eq!(
            client.pending_token_wasm_hash(),
            Some((wasm.clone(), TOKEN_WASM_CHANGE_DELAY_LEDGERS))
        );
        client.cancel_token_wasm_proposal();
        assert_eq!(client.pending_token_wasm_hash(), None);

        // Nothing left to accept, and the factory still has no hash.
        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        assert_eq!(
            client.try_accept_token_wasm_hash(),
            Err(Ok(FactoryError::NoPendingWasmChange.into()))
        );
        assert!(client.try_get_token_wasm_hash().is_err());
    }

    /// Re-proposing restarts the clock: otherwise a hash proposed an hour
    /// ago could be swept in by a second proposal made a minute before the
    /// first one fell due.
    #[test]
    fn test_re_propose_token_wasm_hash_restarts_the_delay() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        let first = dummy_wasm_hash(&env);
        let second = BytesN::from_array(&env, &[9u8; 32]);

        client.propose_token_wasm_hash(&first);
        env.ledger().set_sequence_number(100);
        client.propose_token_wasm_hash(&second);
        assert_eq!(
            client.pending_token_wasm_hash(),
            Some((second.clone(), 100 + TOKEN_WASM_CHANGE_DELAY_LEDGERS))
        );

        // The first proposal's deadline has passed; the second's has not.
        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        assert_eq!(
            client.try_accept_token_wasm_hash(),
            Err(Ok(FactoryError::WasmChangeNotDue.into()))
        );

        env.ledger()
            .set_sequence_number(100 + TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        client.accept_token_wasm_hash();
        assert_eq!(client.get_token_wasm_hash(), second);
    }

    /// Revoking freezes the configuration — including a rotation that was
    /// announced but never accepted.
    #[test]
    fn test_revoke_admin_discards_pending_wasm_proposal() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        let wasm = dummy_wasm_hash(&env);

        client.propose_token_wasm_hash(&wasm);
        client.revoke_admin();
        assert_eq!(client.pending_token_wasm_hash(), None);

        env.ledger()
            .set_sequence_number(TOKEN_WASM_CHANGE_DELAY_LEDGERS);
        assert_eq!(
            client.try_accept_token_wasm_hash(),
            Err(Ok(FactoryError::Locked.into()))
        );
        assert!(client.try_get_token_wasm_hash().is_err());
    }

    // ── Deployment ──────────────────────────────────────────────────────

    #[test]
    fn test_deploy_token_deploys_and_initializes() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, admin) = configured_factory(&env);

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[1u8; 32]);

        let token_address = deploy_token(&env, &client, &deployer, &salt, &admin);

        // Deterministic address derived from deployer + salt.
        let expected = env
            .deployer()
            .with_address(deployer.clone(), salt.clone())
            .deployed_address();
        assert_eq!(token_address, expected);

        // Initialized by the factory in the same invocation.
        let token_client = soroban_token::TokenContractClient::new(&env, &token_address);
        assert_eq!(token_client.name(), String::from_str(&env, "Factory Token"));
        assert_eq!(token_client.symbol(), String::from_str(&env, "FTK"));
        assert_eq!(token_client.decimals(), 7u32);
        assert_eq!(token_client.admin(), admin.clone());
        assert_eq!(token_client.balance(&admin), 1_000_000i128);

        // Recorded in the deployment enum index.
        assert_eq!(client.get_deployment_count(), 1);
        let page = client.get_deployments_paginated(&0u32, &10u32);
        assert_eq!(page.len(), 1);
        assert_eq!(page.get(0).unwrap(), token_address.clone());

        // Emitted a `deploy` event carrying deployer, salt and token address.
        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("deploy"), deployer, salt).into_val(&env),
                    token_address.into_val(&env)
                )
            ]
        );
    }

    #[test]
    fn test_deploy_token_writes_metadata_registry_in_the_same_invocation() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (factory_id, client, admin) = setup(&env);
        set_token_wasm(&env, &client, &dummy_wasm_hash(&env));
        let registry_id = attach_registry(&env, &factory_id, &client, &admin);
        let registry =
            soroban_metadata_registry::MetadataRegistryContractClient::new(&env, &registry_id);

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[8u8; 32]);
        let json = r#"{"name":"Factory Token","symbol":"FTK","description":"hi"}"#;
        let uri = String::from_str(&env, "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi");
        let mut config = default_config(&env, &admin);
        config.metadata = Some(String::from_str(&env, json));
        config.contract_uri = Some(uri.clone());

        register_token_at(&env, &deployer, &salt);
        env.ledger().set_sequence_number(77);
        let token_address = client.deploy_token(&deployer, &salt, &config);

        let token_client = soroban_token::TokenContractClient::new(&env, &token_address);
        assert_eq!(token_client.contract_uri(), Some(uri));

        let record = registry.get_record(&token_address).unwrap();
        assert_eq!(record.symbol, String::from_str(&env, "FTK"));
        assert_eq!(record.creator, deployer);
        assert_eq!(record.launch_ledger, 77);
        assert_eq!(record.initial_supply, 1_000_000i128);
        let expected_name = env
            .crypto()
            .keccak256(&soroban_sdk::Bytes::from_slice(
                &env,
                b"Factory Token",
            ))
            .into();
        let expected_meta = env
            .crypto()
            .keccak256(&soroban_sdk::Bytes::from_slice(&env, json.as_bytes()))
            .into();
        assert_eq!(record.name_digest, expected_name);
        assert_eq!(record.metadata_digest, Some(expected_meta));
        assert_eq!(registry.get_record_count(), 1);
    }

    #[test]
    fn test_deploy_token_requires_metadata_registry() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        set_token_wasm(&env, &client, &dummy_wasm_hash(&env));

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[9u8; 32]);
        register_token_at(&env, &deployer, &salt);

        assert_eq!(
            client.try_deploy_token(&deployer, &salt, &default_config(&env, &admin)),
            Err(Ok(FactoryError::MetadataRegistryNotSet.into()))
        );
        assert_eq!(client.get_deployment_count(), 0);
    }

    #[test]
    fn test_get_deployment_address_is_deterministic() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, admin) = configured_factory(&env);

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[5u8; 32]);

        let predicted = client.get_deployment_address(&deployer, &salt);
        let actual = deploy_token(&env, &client, &deployer, &salt, &admin);
        assert_eq!(predicted, actual);
    }

    #[test]
    fn test_deploy_token_requires_set_wasm_hash() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[4u8; 32]);

        assert!(deploy_token_fails(
            &env, &client, &deployer, &salt, &admin, 7, None
        ));
    }

    #[test]
    fn test_deploy_token_requires_deployer_auth() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (contract_id, client, admin) = setup(&env);
        set_token_wasm(&env, &client, &dummy_wasm_hash(&env));

        let deployer = Address::generate(&env);
        let attacker = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[3u8; 32]);

        let config = TokenConfig {
            admin: admin.clone(),
            decimal: 7,
            name: String::from_str(&env, "T"),
            symbol: String::from_str(&env, "T"),
            initial_supply: 1i128,
            max_supply: None,
            authorization_required: false,
            authorization_revocable: false,
            compliance_node: None,
            metadata: None,
            contract_uri: None,
        };

        // Only the attacker can auth — never the deployer.
        env.mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "deploy_token",
                args: (deployer.clone(), salt.clone(), config.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(client.try_deploy_token(&deployer, &salt, &config).is_err());
    }

    #[test]
    fn test_deploy_token_reverts_atomically_on_initialize_failure() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();

        let (client, admin) = configured_factory(&env);

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[2u8; 32]);

        // decimal 19 is rejected by the token's own initialize validation —
        // the failure happens *after* the deploy step, which is exactly the
        // window the atomic design must close.
        assert!(deploy_token_fails(
            &env, &client, &deployer, &salt, &admin, 19, None
        ));

        // Nothing was recorded...
        assert_eq!(client.get_deployment_count(), 0);
        assert_eq!(client.get_deployments_paginated(&0u32, &10u32).len(), 0);

        // ...and no `deploy` event was emitted — `_finalize_deployment`
        // reverted along with the failed initialize instead of recording a
        // half-initialized token.
        let events = env.events().all();
        let mut deploy_events = 0;
        for i in 0..events.len() {
            let (_, topic, _) = events.get(i).unwrap();
            let topic_0: Symbol = TryFromVal::try_from_val(&env, &topic.get(0).unwrap()).unwrap();
            if topic_0 == symbol_short!("deploy") {
                deploy_events += 1;
            }
        }
        assert_eq!(deploy_events, 0);
    }

    // ── Deployment index ────────────────────────────────────────────────

    #[test]
    fn test_get_deployments_paginated_returns_pages() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, admin) = configured_factory(&env);

        let deployer = Address::generate(&env);
        let mut deployed = soroban_sdk::Vec::new(&env);
        for i in 1u8..4 {
            let salt = BytesN::from_array(&env, &[i; 32]);
            deployed.push_back(deploy_token(&env, &client, &deployer, &salt, &admin));
        }

        assert_eq!(client.get_deployment_count(), 3);

        let page1 = client.get_deployments_paginated(&0u32, &2u32);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap(), deployed.get(0).unwrap());
        assert_eq!(page1.get(1).unwrap(), deployed.get(1).unwrap());

        let page2 = client.get_deployments_paginated(&2u32, &2u32);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2.get(0).unwrap(), deployed.get(2).unwrap());

        // Out-of-range and empty pages.
        assert_eq!(client.get_deployments_paginated(&3u32, &2u32).len(), 0);
        assert_eq!(client.get_deployments_paginated(&0u32, &0u32).len(), 0);
    }

    // ── MAX_PAGE on get_deployments_paginated (#469) ─────────────────────

    #[test]
    fn test_get_deployments_paginated_clamps_an_oversized_limit() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, _admin) = configured_factory(&env);

        // More deployments than one page can hold, so the clamp is observable
        // rather than masked by a short list. Recorded straight into the index
        // rather than by deploying 110 real tokens: the point here is what the
        // getter does with the caller's `limit`, and that must not be
        // contingent on the cost of populating the list.
        let contract_id = client.address.clone();
        let count = MAX_PAGE + 5;
        let mut recorded = soroban_sdk::Vec::new(&env);
        for _ in 0..count {
            let token = Address::generate(&env);
            recorded.push_back(token.clone());
            env.as_contract(&contract_id, || {
                FactoryContract::_record_deployment(&env, &token);
            });
        }
        assert_eq!(client.get_deployment_count(), count);

        // The oversized request is served as a MAX_PAGE-sized page rather than
        // rejected or honoured — this is the fix: `limit` is the caller's, and
        // it must not decide how much the host allocates.
        let clamped = client.get_deployments_paginated(&0u32, &4_000_000_000u32);
        assert_eq!(clamped.len(), MAX_PAGE);
        for i in 0..MAX_PAGE {
            assert_eq!(clamped.get(i).unwrap(), recorded.get(i).unwrap());
        }

        // An explicit `MAX_PAGE` request behaves identically — the clamp is not
        // a special case, it is the same value.
        assert_eq!(
            client.get_deployments_paginated(&0u32, &MAX_PAGE).len(),
            MAX_PAGE
        );

        // A limit below the ceiling is honoured as-is, so the clamp only ever
        // lowers the page size, never raises it.
        assert_eq!(client.get_deployments_paginated(&0u32, &7u32).len(), 7);

        // The remainder is still reachable, so clamping costs a caller nothing
        // but one extra call — nothing is dropped.
        let rest = client.get_deployments_paginated(&MAX_PAGE, &MAX_PAGE);
        assert_eq!(rest.len(), 5);
        assert_eq!(rest.get(0).unwrap(), recorded.get(MAX_PAGE).unwrap());

        let all: u32 = (0..=1)
            .map(|p| {
                client
                    .get_deployments_paginated(&(p * MAX_PAGE), &u32::MAX)
                    .len()
            })
            .sum();
        assert_eq!(all, count);
    // ── Admin lifecycle ─────────────────────────────────────────────────
    //
    // Mirrors the token contract's admin suite (see
    // docs/admin-capability-matrix.md): two-step transfer with an expiry,
    // revoke_admin, pause/unpause, upgrade — each with its own event.

    #[test]
    fn test_propose_and_accept_admin() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        // The old admin keeps the role until the new one accepts.
        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.pending_admin(), Some(next.clone()));

        client.accept_admin();
        assert_eq!(client.get_admin(), next);
        assert_eq!(client.pending_admin(), None);
    }

    #[test]
    fn test_propose_admin_overwrites_previous() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        let first = Address::generate(&env);
        let second = Address::generate(&env);

        client.propose_admin(&first);
        client.propose_admin(&second);
        assert_eq!(client.pending_admin(), Some(second.clone()));

        client.accept_admin();
        assert_eq!(client.get_admin(), second);
    }

    #[test]
    fn test_propose_admin_rejects_current_admin() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);

        assert_eq!(
            client.try_propose_admin(&admin),
            Err(Ok(FactoryError::InvalidAdmin.into()))
        );
    }

    #[test]
    fn test_cancel_admin_proposal_clears_pending_state() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        let first = Address::generate(&env);
        let second = Address::generate(&env);

        client.propose_admin(&first);
        client.propose_admin(&second);
        client.cancel_admin_proposal();

        assert_eq!(client.pending_admin(), None);
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_accept_admin_without_proposal() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        assert_eq!(
            client.try_accept_admin(),
            Err(Ok(FactoryError::NoPendingAdmin.into()))
        );
    }

    #[test]
    fn test_accept_admin_rejects_expired_proposal() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        // Exactly the expiry ledger: `>=` makes it lapsed, and one ledger
        // further would archive the instance entry first (the entry's own
        // TTL is the same constant), so this is the only observable edge.
        env.ledger()
            .set_sequence_number(ADMIN_PROPOSAL_EXPIRY_LEDGERS);

        assert_eq!(
            client.try_accept_admin(),
            Err(Ok(FactoryError::ProposalExpired.into()))
        );
        assert_eq!(client.get_admin(), admin);
    }

    /// The getter clears a lapsed proposal rather than reporting an address
    /// that `accept_admin` would then refuse — stale state must not linger.
    #[test]
    fn test_pending_admin_getter_clears_expired_proposal() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        env.ledger()
            .set_sequence_number(ADMIN_PROPOSAL_EXPIRY_LEDGERS);
        assert_eq!(client.pending_admin(), None);

        // Re-proposing works: expiry is a deadline, not a lockout.
        env.ledger().set_sequence_number(0);
        client.propose_admin(&next);
        assert_eq!(client.pending_admin(), Some(next));
    }

    #[test]
    fn test_old_admin_retains_role_until_accepted() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        assert_eq!(client.get_admin(), admin);
        client.pause();
        assert!(client.is_paused());
        client.unpause();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_accept_admin_blocked_when_paused() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        client.pause();

        assert_eq!(
            client.try_accept_admin(),
            Err(Ok(FactoryError::Paused.into()))
        );
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_propose_admin_requires_admin_auth() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (contract_id, client, _) = setup(&env);
        let stranger = Address::generate(&env);
        let next = Address::generate(&env);

        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "propose_admin",
                args: (next.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(client.try_propose_admin(&next).is_err());
        assert_eq!(client.pending_admin(), None);
    }

    #[test]
    fn test_revoke_admin_sets_locked_flag() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        assert!(!client.is_locked());

        client.revoke_admin();

        assert!(client.is_locked());
    }

    #[test]
    fn test_get_admin_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        client.revoke_admin();

        assert_eq!(client.try_get_admin(), Err(Ok(FactoryError::Locked.into())));
    }

    /// Every admin operation must be permanently unreachable after revoke.
    #[test]
    fn test_admin_operations_after_revoke_fail() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);
        let next = Address::generate(&env);
        client.revoke_admin();

        assert_eq!(
            client.try_propose_admin(&next),
            Err(Ok(FactoryError::Locked.into()))
        );
        assert_eq!(
            client.try_propose_token_wasm_hash(&dummy_wasm_hash(&env)),
            Err(Ok(FactoryError::Locked.into()))
        );
        assert_eq!(
            client.try_cancel_token_wasm_proposal(),
            Err(Ok(FactoryError::Locked.into()))
        );
        assert_eq!(
            client.try_accept_token_wasm_hash(),
            Err(Ok(FactoryError::Locked.into()))
        );
        assert_eq!(client.try_pause(), Err(Ok(FactoryError::Locked.into())));
        assert_eq!(
            client.try_upgrade(&BytesN::from_array(&env, &[1u8; 32])),
            Err(Ok(FactoryError::Locked.into()))
        );
        assert_eq!(
            client.try_accept_admin(),
            Err(Ok(FactoryError::Locked.into()))
        );
    }

    /// `Initialized` is deliberately never cleared by `revoke_admin`, so
    /// revoking cannot re-open `initialize`.
    #[test]
    fn test_initialize_after_revoke_still_fails() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        client.revoke_admin();

        assert_eq!(
            client.try_initialize(&admin),
            Err(Ok(FactoryError::AlreadyInitialized.into()))
        );
    }

    /// Revoking freezes the configuration, not the service: the recorded
    /// WASM hash keeps deploying tokens.
    #[test]
    fn test_deploy_token_still_works_after_revoke() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, admin) = configured_factory(&env);

        client.revoke_admin();

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[9u8; 32]);
        let token_address = deploy_token(&env, &client, &deployer, &salt, &admin);

        assert_eq!(client.get_deployment_count(), 1);
        let token_client = soroban_token::TokenContractClient::new(&env, &token_address);
        assert_eq!(token_client.admin(), admin);
    }

    #[test]
    fn test_pause_blocks_deploy_token() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, admin) = configured_factory(&env);

        let deployer = Address::generate(&env);
        let salt = BytesN::from_array(&env, &[6u8; 32]);
        register_token_at(&env, &deployer, &salt);

        client.pause();
        assert!(client.is_paused());
        assert!(client
            .try_deploy_token(&deployer, &salt, &default_config(&env, &admin))
            .is_err());
        assert_eq!(client.get_deployment_count(), 0);

        client.unpause();
        assert!(!client.is_paused());
        client.deploy_token(&deployer, &salt, &default_config(&env, &admin));
        assert_eq!(client.get_deployment_count(), 1);
    }

    /// Policy setters and read-only getters stay available while paused —
    /// a halted factory can still be re-pointed at a fixed build, it just
    /// cannot deploy from one until it is unpaused.
    #[test]
    fn test_token_wasm_rotation_works_while_paused() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);

        client.pause();
        let wasm = dummy_wasm_hash(&env);
        set_token_wasm(&env, &client, &wasm);

        assert_eq!(client.get_token_wasm_hash(), wasm);
        assert!(client.is_paused());
        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_deployment_count(), 0);
    }

    #[test]
    fn test_non_admin_cannot_pause() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (contract_id, client, _) = setup(&env);
        let stranger = Address::generate(&env);

        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "pause",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(client.try_pause().is_err());
        assert!(!client.is_paused());
    }

    #[test]
    fn test_non_admin_cannot_upgrade() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (contract_id, client, _) = setup(&env);
        let stranger = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "upgrade",
                args: (hash.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(client.try_upgrade(&hash).is_err());
    }

    #[test]
    fn test_upgrade_rejects_zero_hash() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, _) = setup(&env);

        assert_eq!(
            client.try_upgrade(&BytesN::from_array(&env, &[0u8; 32])),
            Err(Ok(FactoryError::InvalidWasmHash.into()))
        );
    }

    /// The admin lifecycle emits one event per action, with the same topic-0
    /// symbols the token contract uses so a shared dashboard admin panel can
    /// render both.
    #[test]
    fn test_admin_lifecycle_emits_an_event_per_action() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (_, client, admin) = setup(&env);
        let next = Address::generate(&env);
        let contract = client.address.clone();

        type Event = (
            soroban_sdk::Address,
            soroban_sdk::Vec<soroban_sdk::Val>,
            soroban_sdk::Val,
        );

        /// The single most recent event, wrapped in a `Vec` so it compares
        /// through `Vec`'s host-side `PartialEq` (`Val` itself has none).
        fn last_event(env: &Env) -> soroban_sdk::Vec<Event> {
            let events = env.events().all();
            events.slice(events.len() - 1..)
        }

        client.propose_admin(&next);
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    contract.clone(),
                    (symbol_short!("prop_adm"), admin.clone(), next.clone()).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );

        client.cancel_admin_proposal();
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    contract.clone(),
                    (symbol_short!("cncl_adm"),).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );

        client.propose_admin(&next);
        client.accept_admin();
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    contract.clone(),
                    (symbol_short!("set_admin"), admin.clone(), next.clone()).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );
        assert_eq!(client.get_admin(), next.clone());

        client.pause();
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    contract.clone(),
                    (symbol_short!("pause"),).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );

        client.unpause();
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    contract.clone(),
                    (symbol_short!("unpause"),).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );

        client.revoke_admin();
        assert_eq!(
            last_event(&env),
            soroban_sdk::vec![
                &env,
                (
                    contract,
                    (symbol_short!("revoked"),).into_val(&env),
                    true.into_val(&env)
                )
            ]
        );
    }
}
