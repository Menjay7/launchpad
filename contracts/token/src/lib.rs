#![no_std]

use soroban_sdk::{
    auth::AuthorizedInvocation, contract, contractclient, contracterror, contractimpl,
    contracttype, panic_with_error, symbol_short, Address, BytesN, Env, String,
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
/// `max_entry_ttl` is 3,110,400 ledgers, so entries actually live **about
/// 180 days, not a year**. Holders who need their balance to outlive that
/// must interact with the contract at least once per window.
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
/// instance kept alive by any other call.
const ADMIN_PROPOSAL_EXPIRY_LEDGERS: u32 = TTL_LEDGERS;

/// Longest contract URI, in bytes, that `initialize` / `update_contract_uri`
/// will store. Comfortably fits an `ipfs://<CIDv1>/metadata.json` or a normal
/// `https://` URL while keeping the instance entry small.
const MAX_CONTRACT_URI_LEN: u32 = 256;

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    PendingAdmin,
    PendingAdminExpiry,
    ComplianceNode,
    Name,
    Symbol,
    Decimals,
    TotalSupply,
    TotalBurned,
    MaxSupply,
    MaxBalancePerAccount,
    ContractUri,
    Balance(Address),
    Allowance(Address, Address), // (owner, spender)
    Frozen(Address),
    IsPaused,
    /// Set to `true` after `revoke_admin` is called. Once locked, no admin
    /// operation (mint, burn_admin, freeze, propose_admin) can
    /// ever succeed again — the token becomes effectively immutable.
    Locked,
    /// Set once on the first successful `initialize` call and never removed.
    /// Unlike `Admin` (which `revoke_admin` deletes), this is the sole
    /// re-initialization guard, so revoking admin can never reopen `initialize`.
    Initialized,
    AuthorizationRequired,
    AuthorizationRevocable,
    AuthorizedHolder(Address),
    /// Set once by `launch_seal`. Holds the issuer's commitment together with
    /// the ledger and total supply at the moment of sealing (#493).
    LaunchSeal,
    /// Continuous-vesting schedule for a recipient, written by `stream_grant`.
    Stream(Address),
    /// Total amount already released to a recipient by `claim_stream`.
    StreamReleased(Address),
}

/// Creator-attested record that a launch reached the issuer's success
/// threshold. Written once by `launch_seal` and never modified.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct LaunchSeal {
    pub commitment: BytesN<32>,
    pub ledger: u32,
    pub supply: i128,
}

#[derive(Clone, Debug)]
#[contracttype]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

/// Continuous-vesting schedule for a single recipient.
///
/// Accrual is pure arithmetic over the ledger sequence: nothing here is
/// touched by the token, so `stream_accrued` reads without a transaction and
/// a paused or frozen token only stops the withdrawal, never the accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Stream {
    pub total: i128,
    pub start_ledger: u32,
    pub end_ledger: u32,
    pub max_per_claim: i128,
    pub released: i128,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed contract errors — surfaced in release WASM as numeric codes.
///
/// Codes 1–3 pre-existed for compliance-node paths and are preserved at those
/// values so that existing clients do not break. The remainder cover every
/// other failure mode so that `try_*` client calls can distinguish failures
/// even in release builds where panic strings are stripped.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TokenError {
    // ── Compliance node ──────────────────────────────────────────────────
    /// The compliance node answered `can_trade` with `false`.
    ComplianceRejected = 1,
    /// The compliance node could not be called or returned a non-`bool`.
    ComplianceNodeUnavailable = 2,
    /// The address passed to `set_compliance_node` failed the probe.
    InvalidComplianceNode = 3,
    // ── Initialization ───────────────────────────────────────────────────
    /// `initialize` was called on a contract that is already initialized.
    AlreadyInitialized = 4,
    /// The contract is permanently locked (`revoke_admin` was called).
    Locked = 5,
    /// The contract is paused.
    Paused = 6,
    // ── Validation ───────────────────────────────────────────────────────
    /// Amount is zero or negative where a positive value is required.
    InvalidAmount = 7,
    /// Sender has insufficient token balance.
    InsufficientBalance = 8,
    /// Spender has insufficient allowance.
    InsufficientAllowance = 9,
    /// The account is frozen and cannot send tokens.
    Frozen = 10,
    /// Recipient is not on the authorized-holders list.
    NotAuthorizedHolder = 11,
    /// `revoke_authorization` was called but authorization is not revocable.
    NotRevocable = 12,
    /// Mint would exceed the `max_supply` cap.
    ExceedsMaxSupply = 13,
    /// WASM hash supplied to `upgrade` is the all-zeros sentinel.
    InvalidWasmHash = 14,
    /// `expiration_ledger` is not strictly greater than the current ledger.
    InvalidLedgerRange = 15,
    /// Transfer or mint would push the recipient above the per-account cap.
    ExceedsMaxBalance = 16,
    /// `accept_admin` was called with no pending proposal.
    NoPendingAdmin = 17,
    /// `initial_supply` exceeds `max_supply` in `initialize`.
    ExceedsInitialSupply = 18,
    /// Decimal value exceeds 18.
    InvalidDecimals = 19,
    /// `mint_batch` received vectors of different lengths.
    BatchLengthMismatch = 20,
    /// `mint_batch` received a batch larger than 100.
    BatchTooLarge = 21,
    /// `contract_uri` getter called before a URI has been set.
    ContractUriNotSet = 22,
    /// A storage getter was called before `initialize`.
    NotInitialized = 23,
    /// `revoke_admin` requires the compliance node to be cleared first.
    ComplianceNodeMustBeCleared = 24,
    /// Contract URI is empty, too long, contains whitespace or control
    /// characters, or uses a scheme other than `https://` or `ipfs://`.
    InvalidContractUri = 25,
    /// `stream_grant` was called with a non-positive total, an end ledger not
    /// strictly after the start ledger, or a non-positive `max_per_claim`.
    InvalidStreamSchedule = 26,
    /// `claim_stream` was called with no schedule for the recipient.
    NoStream = 27,
    /// The authorised invocation does not cover the requested claim range.
    UnauthorizedClaim = 28,
}

#[contractclient(name = "ComplianceNodeClient")]
pub trait ComplianceNodeInterface {
    fn can_trade(env: Env, from: Address, to: Address) -> bool;

    /// Compliance check for issuance (`mint` / `mint_batch`), asked about the
    /// recipient only. Minting has no sending holder, so `can_trade` would
    /// otherwise be asked with the token contract's own address standing in
    /// for `from` — never a KYC'd holder in an allowlist-style node, which
    /// would then reject every mint (see issue #405). Nodes written before
    /// this method existed are not required to implement it: a call that
    /// fails is treated by the token contract as "not implemented" and it
    /// falls back to `can_trade(to, to)` instead.
    fn can_issue(env: Env, to: Address) -> bool;
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// SEP-41 Token Contract — base implementation.
///
/// Contributor issues layered on top:
/// - #1  freeze_account / unfreeze_account (blacklist: no send, no receive, no mint-in)
/// - #2  two-step admin transfer (propose_admin / accept_admin)
/// - #4  max_supply cap enforcement in mint
/// - #138 clawback() and #163 compliance-node transfer checks
/// - #493 launch_seal() graduation signal
#[contract]
pub struct TokenContract;

#[contractimpl]
impl TokenContract {
    // ── Initialization ──────────────────────────────────────────────────

    /// Initialize the token with metadata and an initial supply minted to `admin`.
    ///
    /// `admin.require_auth()` is enforced so the caller must prove they control
    /// the admin address. This prevents a front-runner from setting admin to an
    /// address they do *not* control. The frontend should **always** pass the
    /// deployer's own public key as `admin` so that the wallet's signature
    /// satisfies `require_auth` and the attacker cannot steal the role.
    ///
    /// `authorization_required`: when true, recipients must be explicitly
    /// authorized by the admin before they can receive or hold tokens.
    ///
    /// `authorization_revocable`: when true, the admin may revoke a holder's
    /// authorization, preventing them from receiving further transfers.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        decimal: u32,
        name: String,
        symbol: String,
        initial_supply: i128,
        max_supply: Option<i128>,
        authorization_required: bool,
        authorization_revocable: bool,
        compliance_node: Option<Address>,
        contract_uri: Option<String>,
    ) {
        if env.storage().instance().has(&DataKey::Initialized) {
            panic_with_error!(&env, TokenError::AlreadyInitialized);
        }
        admin.require_auth();
        Self::_require_not_locked(&env);

        if decimal > 18 {
            panic_with_error!(&env, TokenError::InvalidDecimals);
        }

        if let Some(cap) = max_supply {
            if cap <= 0 {
                panic_with_error!(&env, TokenError::InvalidAmount);
            }
            if initial_supply > cap {
                panic_with_error!(&env, TokenError::ExceedsInitialSupply);
            }
            env.storage().instance().set(&DataKey::MaxSupply, &cap);
        }

        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Decimals, &decimal);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::Symbol, &symbol);
        env.storage().instance().set(&DataKey::TotalSupply, &0i128);
        env.storage().instance().set(&DataKey::TotalBurned, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::AuthorizationRequired, &authorization_required);
        env.storage()
            .instance()
            .set(&DataKey::AuthorizationRevocable, &authorization_revocable);
        if let Some(node) = compliance_node {
            env.storage()
                .instance()
                .set(&DataKey::ComplianceNode, &node);
        }
        if let Some(uri) = contract_uri {
            Self::_require_valid_contract_uri(&env, &uri);
            env.storage().instance().set(&DataKey::ContractUri, &uri);
        }

        // When authorization_required is enabled the admin is automatically
        // authorized so the initial supply mint succeeds.
        if authorization_required {
            let key = DataKey::AuthorizedHolder(admin.clone());
            env.storage().persistent().set(&key, &true);
            let ttl_ledgers = Self::_ttl_ledgers(&env);
            env.storage()
                .persistent()
                .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
        }

        if initial_supply > 0 {
            Self::_mint(&env, &admin, initial_supply);
        }

        env.events().publish((symbol_short!("init"),), admin);
    }

    // ── Admin actions ───────────────────────────────────────────────────

    /// Mint `amount` tokens to `to`. Admin only.
    ///
    /// Subject to the compliance node: issuance is a value-moving path, so a
    /// node that rejects `to` blocks the mint. See [`Self::_check_compliance`]
    /// for the scope of the policy.
    pub fn mint(env: Env, to: Address, amount: i128) {
        Self::_check_paused(&env);
        Self::_require_admin(&env);
        if amount <= 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }
        Self::_check_compliance_issue(&env, &to);
        Self::_mint(&env, &to, amount);

        // Extend TTL for the balance key to prevent archiving
        let ttl_ledgers = Self::_ttl_ledgers(&env);
        let key = DataKey::Balance(to);
        env.storage()
            .persistent()
            .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
    }

    /// Burn `amount` tokens from `from`. Owner only (standard burn).
    /// Refuses to run when the account is frozen so a holder cannot
    /// dodge a freeze by destroying tokens.
    pub fn burn(env: Env, from: Address, amount: i128) {
        Self::_check_paused(&env);
        from.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }
        if Self::_is_frozen(&env, &from) {
            panic_with_error!(&env, TokenError::Frozen);
        }
        Self::_burn(&env, &from, amount);
    }

    /// Forced burn of `amount` tokens from `from`. Admin only.
    pub fn burn_admin(env: Env, from: Address, amount: i128) {
        Self::_check_paused(&env);
        Self::_require_admin(&env);
        if amount <= 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }
        Self::_burn(&env, &from, amount);
    }

    /// Forcefully move `amount` tokens from `from` into the admin balance.
    /// Admin only.
    ///
    /// Deliberate freeze bypass: this is the sanctioned recovery path, so it
    /// moves value even when `from` is frozen and even when the admin
    /// recipient would otherwise be gated by the receive-side freeze check.
    /// Every other credit path (transfer, transfer_from, mint) refuses a
    /// frozen recipient; clawback is the single documented exception so an
    /// issuer can always recover funds from a blacklisted account.
    pub fn clawback(env: Env, from: Address, amount: i128) {
        Self::_check_paused(&env);
        Self::_require_admin(&env);
        if amount <= 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }

        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::Locked));
        Self::_check_compliance(&env, &from, &admin);
        Self::_transfer_bypass_frozen(&env, &from, &admin, amount);

        let ttl_ledgers = Self::_ttl_ledgers(&env);
        let from_key = DataKey::Balance(from.clone());
        let admin_key = DataKey::Balance(admin.clone());
        env.storage()
            .persistent()
            .extend_ttl(&from_key, ttl_ledgers, ttl_ledgers);
        env.storage()
            .persistent()
            .extend_ttl(&admin_key, ttl_ledgers, ttl_ledgers);

        env.events()
            .publish((symbol_short!("clawback"), admin, from), amount);
    }

    /// Mint `amount` tokens to multiple recipients. Admin only.
    ///
    /// Maximum batch size is 100 to stay within Soroban's compute budget.
    ///
    /// Each recipient is checked against the compliance node individually, so
    /// one rejected recipient reverts the whole batch. Note that a compliance
    /// node makes the effective batch limit smaller in practice, because every
    /// entry adds a cross-contract call to the invocation's budget.
    pub fn mint_batch(env: Env, to: soroban_sdk::Vec<Address>, amounts: soroban_sdk::Vec<i128>) {
        Self::_check_paused(&env);
        Self::_require_admin(&env);
        if to.len() != amounts.len() {
            panic_with_error!(&env, TokenError::BatchLengthMismatch);
        }
        if to.len() > 100 {
            panic_with_error!(&env, TokenError::BatchTooLarge);
        }
        let ttl_ledgers = Self::_ttl_ledgers(&env);
        for i in 0..to.len() {
            let recipient = to.get(i).unwrap();
            let amount = amounts.get(i).unwrap();
            if amount <= 0 {
                panic_with_error!(&env, TokenError::InvalidAmount);
            }
            Self::_check_compliance_issue(&env, &recipient);
            Self::_mint(&env, &recipient, amount);
            let key = DataKey::Balance(recipient);
            env.storage()
                .persistent()
                .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
        }
    }

    /// Burn `amount` tokens from the caller's own balance. Refuses to
    /// run when the account is frozen so a holder cannot dodge a freeze
    /// by destroying tokens.
    ///
    /// **Alias for `burn`, not a second implementation of it.** `burn` is the
    /// SEP-41 entry point; this name is not, and it is not on the standard
    /// for any wallet to discover. The two were byte-identical including their
    /// doc comments, which made the choice between them arbitrary at the API
    /// surface and doubled the test suite over one body (#107, #468).
    ///
    /// Retained because the launchpad's own UI calls this name
    /// (`buildBurnTransaction` in `frontend/lib/stellar.ts`). Forwarding keeps
    /// that working with `burn` as the single implementation and `burn`'s
    /// suite as the single test suite.
    pub fn burn_self(env: Env, from: Address, amount: i128) {
        Self::burn(env, from, amount)
    }

    /// Propose a new admin. Must be called by the current admin.
    /// The new admin must call `accept_admin` to finalize the transfer.
    pub fn propose_admin(env: Env, new_admin: Address) {
        Self::_require_admin(&env);
        let current_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("admin revoked");
        assert!(new_admin != current_admin, "cannot propose current admin");

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
            .expect("no pending admin");
        let expiry_ledger: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdminExpiry)
            .unwrap_or(0);
        assert!(
            env.ledger().sequence() < expiry_ledger,
            "pending admin proposal expired"
        );

        pending.require_auth();
        let old_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("admin revoked");
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.storage()
            .instance()
            .remove(&DataKey::PendingAdminExpiry);
        env.events()
            .publish((symbol_short!("set_admin"), old_admin, pending), ());
    }

    /// Permanently revoke the admin role and lock the contract.
    ///
    /// After this call:
    /// - No further `mint`, `burn_admin`, `freeze`, `unfreeze`,
    ///   `propose_admin`, `accept_admin`, `pause`, or
    ///   `unpause` operation can ever succeed.
    /// - The Admin storage entry is removed and a `Locked` flag is set.
    /// - `is_locked()` returns `true` from then on.
    ///
    /// A configured compliance node must be cleared first. Its failure mode is
    /// fail-closed, and no administrator remains after this call to clear it.
    /// Once revocation succeeds, holders can still `transfer`, `approve`,
    /// `transfer_from`, `burn`, and `burn_self`; the token becomes trustless /
    /// immutable.
    ///
    /// Any max-balance-per-account cap set via
    /// [`set_max_balance_per_account`](Self::set_max_balance_per_account) is
    /// also deactivated — the cap is only enforced while an admin exists.
    ///
    /// **This action is irreversible.**
    pub fn revoke_admin(env: Env) {
        Self::_require_admin(&env);
        if env.storage().instance().has(&DataKey::ComplianceNode) {
            panic_with_error!(&env, TokenError::ComplianceNodeMustBeCleared);
        }
        env.storage().instance().set(&DataKey::Locked, &true);
        env.storage().instance().remove(&DataKey::Admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.storage()
            .instance()
            .remove(&DataKey::PendingAdminExpiry);
        env.events().publish((symbol_short!("revoked"),), true);
    }

    /// Freeze an account (blacklist): it cannot send or receive tokens, and
    /// the admin cannot mint into it. Admin only.
    ///
    /// Admin [`clawback`](Self::clawback) is the sole exception and may still
    /// pull tokens from a frozen account back to the admin.
    pub fn freeze_account(env: Env, addr: Address) {
        Self::_require_admin(&env);
        let key = DataKey::Frozen(addr.clone());
        env.storage().persistent().set(&key, &true);
        Self::_touch_policy_entry(&env, &key);
        env.events().publish((symbol_short!("freeze"), addr), ());
    }

    /// Unfreeze a previously frozen account. Admin only.
    pub fn unfreeze_account(env: Env, addr: Address) {
        Self::_require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::Frozen(addr.clone()));
        env.events().publish((symbol_short!("unfreeze"), addr), ());
    }

    /// Pause the contract, halting all state-changing operations. Admin only.
    pub fn pause(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().set(&DataKey::IsPaused, &true);
        env.events().publish((symbol_short!("pause"),), ());
    }

    /// Unpause the contract. Admin only.
    pub fn unpause(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().remove(&DataKey::IsPaused);
        env.events().publish((symbol_short!("unpause"),), ());
    }

    /// Grant authorization to `holder`, allowing them to receive tokens when
    /// `authorization_required` is enabled. Admin only.
    pub fn authorize_holder(env: Env, holder: Address) {
        Self::_require_admin(&env);
        let key = DataKey::AuthorizedHolder(holder.clone());
        env.storage().persistent().set(&key, &true);
        Self::_touch_policy_entry(&env, &key);
        env.events()
            .publish((symbol_short!("authorize"), holder), ());
    }

    /// Revoke authorization from `holder`. Only allowed when
    /// `authorization_revocable` is enabled. Admin only.
    pub fn revoke_authorization(env: Env, holder: Address) {
        Self::_require_admin(&env);
        let revocable: bool = env
            .storage()
            .instance()
            .get(&DataKey::AuthorizationRevocable)
            .unwrap_or(false);
        if !revocable {
            panic_with_error!(&env, TokenError::NotRevocable);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::AuthorizedHolder(holder.clone()));
        env.events()
            .publish((symbol_short!("rev_auth"), holder), ());
    }

    /// Returns `true` if `holder` is authorized to receive tokens.
    /// Always returns `true` when `authorization_required` is disabled.
    pub fn is_authorized(env: Env, holder: Address) -> bool {
        let required: bool = env
            .storage()
            .instance()
            .get(&DataKey::AuthorizationRequired)
            .unwrap_or(false);
        if !required {
            return true;
        }
        let key = DataKey::AuthorizedHolder(holder);
        let authorized = env.storage().persistent().get(&key).unwrap_or(false);
        if authorized {
            Self::_touch_policy_entry(&env, &key);
        }
        authorized
    }

    /// Returns `true` if this token requires holders to be authorized before
    /// receiving transfers.
    pub fn authorization_required(env: Env) -> bool {
        if Self::is_locked(env.clone()) {
            return false;
        }
        env.storage()
            .instance()
            .get(&DataKey::AuthorizationRequired)
            .unwrap_or(false)
    }

    /// Returns `true` if the admin may revoke holder authorization.
    pub fn authorization_revocable(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::AuthorizationRevocable)
            .unwrap_or(false)
    }

    /// Enable or disable the authorization-gate policy after deploy. Admin
    /// only. Previously `authorization_required` was written once at
    /// `initialize` with no way to change it later, so a token deployed
    /// without gating could never add it for a later regulated raise, and one
    /// deployed with gating could never turn it off for its holders (issue
    /// #404). `_require_admin` already covers both the admin check and
    /// `_require_not_locked`.
    pub fn set_authorization_required(env: Env, required: bool) {
        Self::_require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::AuthorizationRequired, &required);
        env.events().publish((symbol_short!("set_areq"),), required);
    }

    /// Permanently give up the admin's ability to revoke holder
    /// authorization. One-way only: this can turn `authorization_revocable`
    /// from `true` to `false`, never back. That direction only ever *reduces*
    /// admin power, so it is safe to allow without a second confirmation
    /// step; the reverse (granting revocation power the deploy-time choice
    /// declined) is not offered, matching #404's "one-way" requirement.
    pub fn renounce_authorization_revocable(env: Env) {
        Self::_require_admin(&env);
        let revocable: bool = env
            .storage()
            .instance()
            .get(&DataKey::AuthorizationRevocable)
            .unwrap_or(false);
        assert!(
            revocable,
            "authorization revocation is already permanently disabled"
        );
        env.storage()
            .instance()
            .set(&DataKey::AuthorizationRevocable, &false);
        env.events().publish((symbol_short!("rvk_rvc"),), ());
    }

    /// Set or update the contract URI pointing to off-chain metadata JSON.
    /// Admin only.
    ///
    /// Only `https://` and `ipfs://` URIs of at most `MAX_CONTRACT_URI_LEN`
    /// bytes are accepted; anything else panics with `InvalidContractUri`.
    pub fn update_contract_uri(env: Env, uri: String) {
        Self::_require_admin(&env);
        Self::_require_valid_contract_uri(&env, &uri);
        env.storage().instance().set(&DataKey::ContractUri, &uri);
        env.events().publish((symbol_short!("upd_uri"),), uri);
    }

    /// Upgrade this contract's WASM code hash in place. Admin only.
    ///
    /// Security note: this preserves existing storage and contract address, so
    /// new WASM must remain storage-compatible with previous deployments.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::_require_admin(&env);
        if new_wasm_hash == BytesN::from_array(&env, &[0; 32]) {
            panic_with_error!(&env, TokenError::InvalidWasmHash);
        }
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("upgrade"),), new_wasm_hash);
    }

    // ── Token operations ────────────────────────────────────────────────

    /// Transfer `amount` from `from` to `to`. Caller must be `from`.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        Self::_check_paused(&env);
        from.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }
        if Self::_is_frozen(&env, &from) {
            panic_with_error!(&env, TokenError::Frozen);
        }
        Self::_check_compliance(&env, &from, &to);
        Self::_check_authorized(&env, &to);

        Self::_transfer(&env, &from, &to, amount);

        // Extend TTL for both balance keys to prevent archiving
        // Use a standard TTL extension (e.g., 52 weeks in ledgers)
        let ttl_ledgers = Self::_ttl_ledgers(&env);
        let from_key = DataKey::Balance(from);
        let to_key = DataKey::Balance(to);
        env.storage()
            .persistent()
            .extend_ttl(&from_key, ttl_ledgers, ttl_ledgers);
        env.storage()
            .persistent()
            .extend_ttl(&to_key, ttl_ledgers, ttl_ledgers);
    }

    /// Approve `spender` to spend up to `amount` on behalf of `from`.
    ///
    /// When `amount > 0`, `expiration_ledger` must be strictly greater than
    /// the current ledger sequence. The allowance is stored in temporary
    /// storage and its TTL is clamped to `env.storage().max_ttl()` to avoid
    /// exceeding the network-enforced ceiling.
    ///
    /// When `amount == 0`, the call is treated as a **revocation**: the
    /// allowance entry is removed from storage and `expiration_ledger` is
    /// ignored. This is the canonical, wallet-emitted way to revoke an
    /// allowance (SEP-41 §4.1) and works regardless of the value passed for
    /// `expiration_ledger`.
    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) {
        Self::_check_paused(&env);
        from.require_auth();
        if amount < 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }
        assert!(!Self::_is_frozen(&env, &from), "account is frozen");

        let key = DataKey::Allowance(from.clone(), spender.clone());

        // NOTE: frontend clients (buildRevokeAllowanceArgs) pass expiration_ledger = 0
        // when revoking an allowance.  The 0 is safe because the branch below ignores
        // expiration_ledger when amount == 0.  Do NOT add an assertion on
        // expiration_ledger here without also updating the frontend.
        if amount == 0 {
            env.storage().temporary().remove(&key);
        } else {
            let current_ledger = env.ledger().sequence();
            if expiration_ledger <= current_ledger {
                panic_with_error!(&env, TokenError::InvalidLedgerRange);
            }

            let value = AllowanceValue {
                amount,
                expiration_ledger,
            };
            env.storage().temporary().set(&key, &value);

            let ttl_ledgers = (expiration_ledger - current_ledger).min(env.storage().max_ttl());
            env.storage()
                .temporary()
                .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
        }

        env.events().publish(
            (symbol_short!("approve"), from, spender),
            (amount, expiration_ledger),
        );
    }

    /// Transfer `amount` from `from` to `to` using `spender`'s allowance.
    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        Self::_check_paused(&env);
        spender.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, TokenError::InvalidAmount);
        }
        if Self::_is_frozen(&env, &from) {
            panic_with_error!(&env, TokenError::Frozen);
        }
        Self::_check_compliance(&env, &from, &to);
        Self::_check_authorized(&env, &to);

        let key = DataKey::Allowance(from.clone(), spender.clone());
        let current_ledger = env.ledger().sequence();
        let stored: Option<AllowanceValue> = env.storage().temporary().get(&key);
        let allowance = match &stored {
            Some(v) if v.expiration_ledger >= current_ledger => v.amount,
            _ => 0,
        };
        if allowance < amount {
            panic_with_error!(&env, TokenError::InsufficientAllowance);
        }

        let remaining = allowance - amount;
        let expiration_ledger = stored.expect("allowance checked above").expiration_ledger;
        if remaining > 0 {
            let value = AllowanceValue {
                amount: remaining,
                expiration_ledger,
            };
            env.storage().temporary().set(&key, &value);
            // Clamp to max_ttl so the host does not reject the extend_ttl call
            // for temporary entries whose expiration_ledger was approved past
            // the network ceiling (fixes the partial-spend revert — see #344).
            let ttl_ledgers =
                (expiration_ledger.saturating_sub(current_ledger)).min(env.storage().max_ttl());
            if ttl_ledgers > 0 {
                env.storage()
                    .temporary()
                    .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
            }
        } else {
            env.storage().temporary().remove(&key);
        }

        Self::_transfer(&env, &from, &to, amount);

        // Extend TTL for balance keys to prevent archiving
        let ttl_ledgers = Self::_ttl_ledgers(&env);
        let from_key = DataKey::Balance(from);
        let to_key = DataKey::Balance(to);
        env.storage()
            .persistent()
            .extend_ttl(&from_key, ttl_ledgers, ttl_ledgers);
        env.storage()
            .persistent()
            .extend_ttl(&to_key, ttl_ledgers, ttl_ledgers);
    }

    /// Burn `amount` tokens from `from` using `spender`'s allowance.
    /// Refuses to run when `from` is frozen so a holder cannot dodge a
    /// freeze by having an approved spender destroy their tokens.
    pub fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        Self::_check_paused(&env);
        spender.require_auth();
        assert!(amount > 0, "amount must be positive");
        assert!(!Self::_is_frozen(&env, &from), "account is frozen");

        let key = DataKey::Allowance(from.clone(), spender.clone());
        let current_ledger = env.ledger().sequence();
        let stored: Option<AllowanceValue> = env.storage().temporary().get(&key);
        let allowance = match &stored {
            Some(v) if v.expiration_ledger >= current_ledger => v.amount,
            _ => 0,
        };
        assert!(allowance >= amount, "insufficient allowance");

        let remaining = allowance - amount;
        let expiration_ledger = stored.expect("allowance checked above").expiration_ledger;
        if remaining > 0 {
            let value = AllowanceValue {
                amount: remaining,
                expiration_ledger,
            };
            env.storage().temporary().set(&key, &value);
            // Clamp to max_ttl so the host does not reject the extend_ttl call
            // for temporary entries whose expiration_ledger was approved past
            // the network ceiling (fixes the partial-spend revert — see #344).
            let ttl_ledgers =
                (expiration_ledger.saturating_sub(current_ledger)).min(env.storage().max_ttl());
            if ttl_ledgers > 0 {
                env.storage()
                    .temporary()
                    .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
            }
        } else {
            env.storage().temporary().remove(&key);
        }

        Self::_burn(&env, &from, amount);
    }

    // ── Read-only getters ───────────────────────────────────────────────

    pub fn balance(env: Env, id: Address) -> i128 {
        let key = DataKey::Balance(id);
        env.storage().persistent().get(&key).unwrap_or(0)
    }

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        let key = DataKey::Allowance(from, spender);
        match env
            .storage()
            .temporary()
            .get::<DataKey, AllowanceValue>(&key)
        {
            Some(v) if v.expiration_ledger >= env.ledger().sequence() => v.amount,
            _ => 0,
        }
    }

    pub fn admin(env: Env) -> Address {
        let locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false);
        if locked {
            panic_with_error!(&env, TokenError::Locked);
        }
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::NotInitialized))
    }

    /// Returns the current administrator without treating an intentionally
    /// locked token as an error. `admin()` remains the strict compatibility
    /// view for callers that require an active administrator.
    pub fn admin_if_any(env: Env) -> Option<Address> {
        if Self::is_locked(env.clone()) {
            return None;
        }
        env.storage().instance().get(&DataKey::Admin)
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

    pub fn decimals(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Decimals)
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::NotInitialized))
    }

    pub fn name(env: Env) -> String {
        env.storage()
            .instance()
            .get(&DataKey::Name)
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::NotInitialized))
    }

    pub fn symbol(env: Env) -> String {
        env.storage()
            .instance()
            .get(&DataKey::Symbol)
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::NotInitialized))
    }

    pub fn total_supply(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    pub fn total_burned(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0)
    }

    /// Returns `true` if the given address is frozen.
    pub fn is_frozen(env: Env, addr: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Frozen(addr))
            .unwrap_or(false)
    }

    /// Returns the immutable supply ceiling recorded at initialization.
    /// It remains useful after admin revocation as a historical token fact.
    pub fn max_supply(env: Env) -> Option<i128> {
        env.storage().instance().get(&DataKey::MaxSupply)
    }

    /// Optional whale protection: max balance per account as a percentage of total supply.
    ///
    /// If set to `p`, then for any transfer/mint to a non-admin recipient:
    /// `balance(recipient) <= total_supply * p / 100`.
    ///
    /// The cap is only enforced while an admin exists. After
    /// [`revoke_admin`](Self::revoke_admin) removes the admin the cap becomes
    /// inactive so the token remains fully transferable.
    ///
    /// Once the contract is locked this returns `None` even if a percentage was
    /// stored, so the getter never reports a limit that is not being enforced.
    /// The stored value is deliberately left in place (so a counterfactual
    /// re-init after a future upgrade sees it) but is masked by the lock state.
    pub fn max_balance_per_account(env: Env) -> Option<u32> {
        let locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false);
        if locked {
            return None;
        }
        env.storage().instance().get(&DataKey::MaxBalancePerAccount)
    }

    /// Set the optional max balance per account as a percentage of total supply.
    /// Admin only.
    ///
    /// - `None` disables whale protection
    /// - `Some(p)` enables it, where `p` must be between 1 and 100 (inclusive)
    ///
    /// The cap is only enforced while an admin exists. After
    /// [`revoke_admin`](Self::revoke_admin) removes the admin the cap becomes
    /// inactive so the token remains fully transferable.
    pub fn set_max_balance_per_account(env: Env, max_balance_per_account: Option<u32>) {
        Self::_require_admin(&env);

        if let Some(p) = max_balance_per_account {
            if !(1..=100).contains(&p) {
                panic_with_error!(&env, TokenError::InvalidAmount);
            }
            env.storage()
                .instance()
                .set(&DataKey::MaxBalancePerAccount, &p);
        } else {
            env.storage()
                .instance()
                .remove(&DataKey::MaxBalancePerAccount);
        }

        env.events()
            .publish((symbol_short!("set_max_b"),), max_balance_per_account);
    }

    /// Set, update, or remove the optional compliance node address.
    /// Admin only. Pass `None` to remove the compliance node.
    ///
    /// The candidate address is **probed before it is stored**: the contract
    /// calls `can_trade` on it once with its own address on both sides and
    /// rejects the address with [`TokenError::InvalidComplianceNode`] unless
    /// the call succeeds and returns a `bool`. The probe's answer is ignored —
    /// only its callability matters. This is what stops the common bricking
    /// mistake of pointing the token at a non-contract address, at a contract
    /// without `can_trade`, or at the token's own address (which fails as
    /// re-entry).
    ///
    /// Clearing the node (`None`) never probes anything, so an admin can always
    /// recover from a node that has since been archived or has started failing.
    pub fn set_compliance_node(env: Env, node: Option<Address>) {
        Self::_require_admin(&env);

        if let Some(addr) = node.clone() {
            let probe = env.current_contract_address();
            let client = ComplianceNodeClient::new(&env, &addr);
            match client.try_can_trade(&probe, &probe) {
                // Either answer is fine; the node only has to be callable.
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {
                    panic_with_error!(&env, TokenError::InvalidComplianceNode)
                }
            }
            env.storage()
                .instance()
                .set(&DataKey::ComplianceNode, &addr);
        } else {
            env.storage().instance().remove(&DataKey::ComplianceNode);
        }

        env.events().publish((symbol_short!("set_cnode"),), node);
    }

    /// Returns `true` if the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsPaused)
            .unwrap_or(false)
    }

    /// Returns the contract metadata URI, if one has been configured. The URI
    /// remains readable after locking because it is historical metadata, not
    /// an active administrative policy.
    pub fn contract_uri(env: Env) -> Option<String> {
        env.storage().instance().get(&DataKey::ContractUri)
    }

    /// Returns the configured compliance node, if any.
    pub fn compliance_node(env: Env) -> Option<Address> {
        if Self::is_locked(env.clone()) {
            return None;
        }
        env.storage()
            .instance()
            .get(&DataKey::ComplianceNode)
            .unwrap_or(None)
    }

    // ── Launch seal (#493) ──────────────────────────────────────────────

    /// Record that this launch reached the issuer's definition of success.
    /// Admin only, and can only be called once.
    ///
    /// `commitment` is an opaque 32-byte value chosen by the issuer (e.g. the
    /// hash of a published success criterion). The ledger sequence and total
    /// supply at the moment of sealing are stored alongside it, giving
    /// indexers an objective, on-chain signal to sort launches by.
    pub fn launch_seal(env: Env, commitment: BytesN<32>) {
        Self::_require_admin(&env);
        if env.storage().instance().has(&DataKey::LaunchSeal) {
            panic!("launch already sealed");
        }

        let seal = LaunchSeal {
            commitment,
            ledger: env.ledger().sequence(),
            supply: Self::total_supply(env.clone()),
        };
        env.storage().instance().set(&DataKey::LaunchSeal, &seal);

        env.events().publish(
            (symbol_short!("seal"),),
            (seal.commitment, seal.ledger, seal.supply),
        );
    }

    /// Returns `true` once `launch_seal` has been called.
    pub fn sealed(env: Env) -> bool {
        env.storage().instance().has(&DataKey::LaunchSeal)
    }

    /// Returns the seal record, or `None` if the launch has not been sealed.
    pub fn launch_seal_info(env: Env) -> Option<LaunchSeal> {
        env.storage().instance().get(&DataKey::LaunchSeal)
    }

    // ── Continuous vesting streams (#168) ───────────────────────────────

    /// Create a continuous-vesting stream for `recipient`. Admin only.
    ///
    /// `total` accrues linearly from `start_ledger` to `end_ledger`, and the
    /// recipient may release accrued tokens with `claim_stream` up to
    /// `max_per_claim` per call. The bound is what keeps a single authorised
    /// invocation from draining the contract.
    pub fn stream_grant(
        env: Env,
        recipient: Address,
        total: i128,
        start_ledger: u32,
        end_ledger: u32,
        max_per_claim: i128,
    ) {
        Self::_require_admin(&env);
        if total <= 0 || max_per_claim <= 0 || end_ledger <= start_ledger {
            panic_with_error!(&env, TokenError::InvalidStreamSchedule);
        }

        let stream = Stream {
            total,
            start_ledger,
            end_ledger,
            max_per_claim,
            released: 0,
        };
        let key = DataKey::Stream(recipient.clone());
        env.storage().persistent().set(&key, &stream);
        Self::_touch_policy_entry(&env, &key);

        env.events().publish(
            (symbol_short!("stream"), recipient),
            (total, start_ledger, end_ledger, max_per_claim),
        );
    }

    /// Release accrued tokens to `recipient` against a scoped authorisation.
    ///
    /// The recipient pre-authorises a bounded claim range once, off-chain;
    /// a relayer submits the resulting `AuthorizedInvocation`, and the
    /// contract verifies the scope covers `(recipient, from_ledger,
    /// to_ledger)` before paying `accrued(to_ledger) - released`, capped at
    /// the schedule's `max_per_claim`.
    pub fn claim_stream(
        env: Env,
        recipient: Address,
        from_ledger: u32,
        to_ledger: u32,
        auth: AuthorizedInvocation,
    ) {
        Self::_check_paused(&env);

        let key = DataKey::Stream(recipient.clone());
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::NoStream));

        // The recipient's signature must cover exactly this claim range.
        Self::_require_claim_authorized(&env, &recipient, from_ledger, to_ledger, &auth);

        let accrued_to = Self::_stream_accrued_at(&stream, to_ledger);
        let claimable = accrued_to
            .checked_sub(stream.released)
            .expect("stream released exceeds accrued");
        if claimable <= 0 {
            return;
        }
        let payout = claimable.min(stream.max_per_claim);

        stream.released = stream
            .released
            .checked_add(payout)
            .expect("stream released overflow");
        env.storage().persistent().set(&key, &stream);
        Self::_touch_policy_entry(&env, &key);

        Self::_mint(&env, &recipient, payout);
        let ttl_ledgers = Self::_ttl_ledgers(&env);
        let balance_key = DataKey::Balance(recipient.clone());
        env.storage()
            .persistent()
            .extend_ttl(&balance_key, ttl_ledgers, ttl_ledgers);

        env.events().publish(
            (symbol_short!("claim"), recipient),
            (payout, from_ledger, to_ledger),
        );
    }

    /// Pure-arithmetic view of how much has accrued to `recipient` by
    /// `ledger`. Never touches the token, so it is safe to call while the
    /// contract is paused or the recipient is frozen (#168).
    pub fn stream_accrued(env: Env, recipient: Address, ledger: u32) -> i128 {
        let stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(recipient))
            .unwrap_or_else(|| panic_with_error!(&env, TokenError::NoStream));
        Self::_stream_accrued_at(&stream, ledger)
    }

    /// Returns the schedule for `recipient`, or `None` if none exists.
    pub fn stream_info(env: Env, recipient: Address) -> Option<Stream> {
        env.storage().persistent().get(&DataKey::Stream(recipient))
    }

    /// Linear accrual of `stream.total` between `start_ledger` and
    /// `end_ledger`, clamped at both ends. Pure arithmetic.
    fn _stream_accrued_at(stream: &Stream, ledger: u32) -> i128 {
        if ledger <= stream.start_ledger {
            return 0;
        }
        if ledger >= stream.end_ledger {
            return stream.total;
        }
        let elapsed = (ledger - stream.start_ledger) as i128;
        let duration = (stream.end_ledger - stream.start_ledger) as i128;
        stream
            .total
            .checked_mul(elapsed)
            .expect("stream accrual overflow")
            / duration
    }

    /// Verify that `auth` is a `claim_stream` invocation scoped to
    /// `(recipient, from_ledger, to_ledger)` and that `recipient` signed it.
    fn _require_claim_authorized(
        env: &Env,
        recipient: &Address,
        from_ledger: u32,
        to_ledger: u32,
        auth: &AuthorizedInvocation,
    ) {
        let expected_args = (
            recipient.clone(),
            from_ledger,
            to_ledger,
        )
            .into_val(env);
        let matches = auth.function == symbol_short!("claim_str")
            && auth.contract == env.current_contract_address()
            && auth.args == expected_args;
        if !matches {
            panic_with_error!(env, TokenError::UnauthorizedClaim);
        }
        recipient.require_auth_for_args(expected_args);
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn _check_authorized(env: &Env, holder: &Address) {
        if Self::is_locked(env.clone()) {
            return;
        }
        let required: bool = env
            .storage()
            .instance()
            .get(&DataKey::AuthorizationRequired)
            .unwrap_or(false);
        if required {
            let key = DataKey::AuthorizedHolder(holder.clone());
            let authorized: bool = env.storage().persistent().get(&key).unwrap_or(false);
            if !authorized {
                panic_with_error!(env, TokenError::NotAuthorizedHolder);
            }
            Self::_touch_policy_entry(env, &key);
        }
    }

    fn _require_admin(env: &Env) {
        Self::_require_not_locked(env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, TokenError::Locked));
        admin.require_auth();
    }

    /// Reject any contract URI that is not a bounded `https://` or `ipfs://`
    /// URI. Frontends render this value as a link, so a `javascript:` or
    /// `data:` URI stored here would be a stored XSS against every visitor.
    /// The scheme match is ASCII case-insensitive, as URI schemes are, and
    /// whitespace / control bytes are refused anywhere in the string.
    fn _require_valid_contract_uri(env: &Env, uri: &String) {
        let len = uri.len();
        if len == 0 || len > MAX_CONTRACT_URI_LEN {
            panic_with_error!(env, TokenError::InvalidContractUri);
        }
        let mut buf = [0u8; MAX_CONTRACT_URI_LEN as usize];
        let bytes = &mut buf[..len as usize];
        uri.copy_into_slice(bytes);

        let has_scheme = |scheme: &[u8]| {
            bytes.len() > scheme.len() && bytes[..scheme.len()].eq_ignore_ascii_case(scheme)
        };
        let allowed_scheme = has_scheme(b"https://") || has_scheme(b"ipfs://");
        let printable = bytes.iter().all(|b| *b > 0x20 && *b != 0x7f);
        if !allowed_scheme || !printable {
            panic_with_error!(env, TokenError::InvalidContractUri);
        }
    }

    /// The TTL to request for a persistent entry: our desired window, capped
    /// at what the network will actually honour.
    ///
    /// `soroban-env-host` silently lowers an over-long `extend_ttl` on a
    /// *persistent* entry rather than erroring, so an unclamped call is not a
    /// hard failure — it just quietly gets you a shorter entry than the code
    /// appears to ask for. Clamping here keeps the requested and effective
    /// values the same, so the archival window is legible from the source.
    fn _ttl_ledgers(env: &Env) -> u32 {
        TTL_LEDGERS.min(env.storage().max_ttl())
    }

    /// Keep a persistent compliance-policy entry alive after it is written or
    /// successfully read. Blacklist and allowlist keys share this helper so a
    /// new policy cannot silently fall back to the host's short minimum TTL.
    fn _touch_policy_entry(env: &Env, key: &DataKey) {
        let ttl_ledgers = Self::_ttl_ledgers(env);
        env.storage()
            .persistent()
            .extend_ttl(key, ttl_ledgers, ttl_ledgers);
    }

    fn _require_not_locked(env: &Env) {
        let locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false);
        if locked {
            panic_with_error!(env, TokenError::Locked);
        }
    }

    fn _is_frozen(env: &Env, addr: &Address) -> bool {
        let key = DataKey::Frozen(addr.clone());
        let frozen = env.storage().persistent().get(&key).unwrap_or(false);
        if frozen {
            Self::_touch_policy_entry(env, &key);
        }
        frozen
    }

    fn _check_paused(env: &Env) {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::IsPaused)
            .unwrap_or(false)
        {
            panic_with_error!(env, TokenError::Paused);
        }
    }

    fn _enforce_max_balance_per_account(env: &Env, to: &Address, new_balance: i128, supply: i128) {
        let Some(pct) = env
            .storage()
            .instance()
            .get::<DataKey, u32>(&DataKey::MaxBalancePerAccount)
        else {
            return;
        };

        let Some(admin) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::Admin)
        else {
            return; // contract is locked (admin revoked); cap no longer enforced
        };

        if to == &admin {
            return; // the admin is exempt from the cap
        }

        let max_allowed = supply
            .checked_mul(pct as i128)
            .expect("max balance calc overflow")
            / 100i128;

        if new_balance > max_allowed {
            panic_with_error!(env, TokenError::ExceedsMaxBalance);
        }
    }
    /// Ask the configured compliance node whether `from` → `to` is permitted.
    ///
    /// No-op when no node is configured. When one is configured the call is
    /// made with `try_can_trade`, so a node that panics, has been archived, no
    /// longer exists, or answers with a non-`bool` is contained and reported as
    /// [`TokenError::ComplianceNodeUnavailable`] rather than letting a raw host
    /// error escape.
    ///
    /// The policy is **fail closed**: an unreachable node blocks value-moving
    /// operations. Recovery is always available because `set_compliance_node`
    /// can clear the node while an admin exists.
    ///
    /// # Policy scope
    ///
    /// | Operation | Checked | Why |
    /// | --- | --- | --- |
    /// | `transfer`, `transfer_from` | yes | holder-to-holder value movement |
    /// | `mint`, `mint_batch` | yes | issuance into a recipient the node may reject |
    /// | `clawback` | yes | forced holder-to-admin value movement |
    /// | `burn`, `burn_admin`, `burn_self` | no | destroys tokens; there is no recipient to gate, and gating burns would let a failing node trap holders' balances |
    fn _check_compliance(env: &Env, from: &Address, to: &Address) {
        if Self::is_locked(env.clone()) {
            return;
        }
        let compliance_node: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::ComplianceNode)
            .unwrap_or(None);

        let Some(node) = compliance_node else {
            return;
        };

        let client = ComplianceNodeClient::new(env, &node);
        match client.try_can_trade(from, to) {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => panic_with_error!(env, TokenError::ComplianceRejected),
            // `Ok(Err(_))` → the node returned something that is not a `bool`.
            // `Err(_)`     → the node panicked, is missing, or is not a contract.
            Ok(Err(_)) | Err(_) => panic_with_error!(env, TokenError::ComplianceNodeUnavailable),
        }
    }

    /// Compliance check for issuance. Asks the node's `can_issue(to)` — not
    /// `can_trade` with the token contract's own address standing in for
    /// `from`, which an allowlist-style node would always reject since the
    /// token contract itself is never a KYC'd holder (issue #405). A node
    /// that does not implement `can_issue` (any pre-#405 node) is treated as
    /// answering via `can_trade(to, to)` instead, so existing deployments
    /// keep working without a redeploy.
    fn _check_compliance_issue(env: &Env, to: &Address) {
        if Self::is_locked(env.clone()) {
            return;
        }
        let compliance_node: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::ComplianceNode)
            .unwrap_or(None);

        let Some(node) = compliance_node else {
            return;
        };

        let client = ComplianceNodeClient::new(env, &node);
        match client.try_can_issue(to) {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => panic_with_error!(env, TokenError::ComplianceRejected),
            // Outer `Err` covers a node that has no `can_issue` export at
            // all — the expected shape for every node deployed before this
            // method existed. Fall back to asking about the recipient via
            // `can_trade(to, to)`, the same substitute the issue prescribes.
            Ok(Err(_)) | Err(_) => match client.try_can_trade(to, to) {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => panic_with_error!(env, TokenError::ComplianceRejected),
                Ok(Err(_)) | Err(_) => {
                    panic_with_error!(env, TokenError::ComplianceNodeUnavailable)
                }
            },
        }
    }

    fn _mint(env: &Env, to: &Address, amount: i128) {
        // Receive-side freeze: issuance into a blacklisted account is exactly
        // the accumulation a freeze exists to prevent, so it is refused here.
        // Clawback does not mint, so admin recovery is unaffected.
        assert!(!Self::_is_frozen(env, to), "account is frozen");
        Self::_check_authorized(env, to);
        let supply: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        let new_supply = supply.checked_add(amount).expect("total_supply overflow");

        if let Some(cap) = env
            .storage()
            .instance()
            .get::<DataKey, i128>(&DataKey::MaxSupply)
        {
            if new_supply > cap {
                panic_with_error!(env, TokenError::ExceedsMaxSupply);
            }
        }

        let key = DataKey::Balance(to.clone());
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = balance.checked_add(amount).expect("balance overflow");

        Self::_enforce_max_balance_per_account(env, to, new_balance, supply);

        env.storage().persistent().set(&key, &new_balance);

        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &new_supply);

        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| env.current_contract_address());
        env.events()
            .publish((symbol_short!("mint"), admin, to.clone()), amount);
    }

    /// Consume `amount` of `spender`'s allowance on `from`.
    ///
    /// `transfer_from` and `burn_from` differ only in what they do with the
    /// tokens afterwards, but the whole allowance path — the guard, the
    /// temporary-storage read, the expiry comparison, the remaining-allowance
    /// rewrite, the `max_ttl` clamp and the zero-case removal — is identical
    /// and used to be written out twice. That duplication is not cosmetic: it
    /// is why the #344 `max_ttl` clamp had to be applied in two places, and
    /// it means any future fix to this path has to be remembered in both
    /// functions (issue #467).
    ///
    /// An expired or absent entry reads as an allowance of zero, per SEP-41
    /// ("an expired entry … should be treated as a 0 amount allowance"), so
    /// both the `allowance` getter and this spend agree on the same rule.
    ///
    /// **Precondition:** the caller has already rejected `amount <= 0`. That
    /// is what keeps the `expect` honest — with no entry the allowance is 0,
    /// and 0 is already `< amount`, so `InsufficientAllowance` is raised
    /// before the unwrap can be reached. A future caller that skips the
    /// positive-amount check would turn it into a panic instead.
    ///
    /// Note this preserves `expiration_ledger` on a partial spend, so a
    /// spender cannot extend an allowance's life by spending it — SEP-41:
    /// "Reduces the allowance by `amount` without changing when it expires."
    fn _spend_allowance(env: &Env, spender: &Address, from: &Address, amount: i128) {
        let key = DataKey::Allowance(from.clone(), spender.clone());
        let current_ledger = env.ledger().sequence();
        let stored: Option<AllowanceValue> = env.storage().temporary().get(&key);
        let allowance = match &stored {
            Some(v) if v.expiration_ledger >= current_ledger => v.amount,
            _ => 0,
        };
        if allowance < amount {
            panic_with_error!(env, TokenError::InsufficientAllowance);
        }

        let remaining = allowance - amount;
        let expiration_ledger = stored.expect("allowance checked above").expiration_ledger;
        if remaining > 0 {
            let value = AllowanceValue {
                amount: remaining,
                expiration_ledger,
            };
            env.storage().temporary().set(&key, &value);
            // Clamp to max_ttl so the host does not reject the extend_ttl call
            // for temporary entries whose expiration_ledger was approved past
            // the network ceiling (fixes the partial-spend revert — see #344).
            let ttl_ledgers =
                (expiration_ledger.saturating_sub(current_ledger)).min(env.storage().max_ttl());
            if ttl_ledgers > 0 {
                env.storage()
                    .temporary()
                    .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
            }
        } else {
            // Spending the allowance down to zero is a revocation, so the
            // entry is removed rather than left as a 0-amount, still-dated
            // record.
            env.storage().temporary().remove(&key);
        }
    }

    fn _burn(env: &Env, from: &Address, amount: i128) {
        let key = DataKey::Balance(from.clone());
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if balance < amount {
            panic_with_error!(env, TokenError::InsufficientBalance);
        }
        let new_balance = balance
            .checked_sub(amount)
            .expect("balance underflow on burn");
        env.storage().persistent().set(&key, &new_balance);

        let supply: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        let new_supply = supply
            .checked_sub(amount)
            .expect("total_supply underflow on burn");
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &new_supply);

        let burned: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0);
        let new_total_burned = burned.checked_add(amount).expect("total_burned overflow");
        env.storage()
            .instance()
            .set(&DataKey::TotalBurned, &new_total_burned);

        env.events()
            .publish((symbol_short!("burn"), from.clone()), amount);
    }

    /// Public transfer path. Enforces the receive-side freeze: a frozen
    /// recipient cannot be credited. Admin recovery goes through
    /// [`Self::_transfer_bypass_frozen`] instead, so a blacklisted account's
    /// tokens can still be clawed back.
    fn _transfer(env: &Env, from: &Address, to: &Address, amount: i128) {
        assert!(!Self::_is_frozen(env, to), "account is frozen");
        Self::_transfer_internal(env, from, to, amount);
    }

    /// Transfer that deliberately skips the recipient freeze check. Only
    /// reachable from [`Self::clawback`], whose whole purpose is to move value
    /// out of a frozen account back to the admin. `clawback` already guards
    /// that the recipient is the caller-controlled admin, so this bypass
    /// cannot be used to credit an arbitrary frozen third party.
    fn _transfer_bypass_frozen(env: &Env, from: &Address, to: &Address, amount: i128) {
        Self::_transfer_internal(env, from, to, amount);
    }

    /// Shared balance-movement core for both the gated and bypass paths.
    /// Callers are responsible for any freeze / compliance / authorization
    /// checks appropriate to their path before calling this.
    fn _transfer_internal(env: &Env, from: &Address, to: &Address, amount: i128) {
        let from_key = DataKey::Balance(from.clone());
        let to_key = DataKey::Balance(to.clone());

        let from_balance: i128 = env.storage().persistent().get(&from_key).unwrap_or(0);
        if from_balance < amount {
            panic_with_error!(env, TokenError::InsufficientBalance);
        }

        env.storage()
            .persistent()
            .set(&from_key, &(from_balance - amount));

        let ttl_ledgers = Self::_ttl_ledgers(&env);
        env.storage()
            .persistent()
            .extend_ttl(&from_key, ttl_ledgers, ttl_ledgers);

        let to_balance: i128 = env.storage().persistent().get(&to_key).unwrap_or(0);

        let supply: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);

        let new_to_balance = to_balance.checked_add(amount).expect("balance overflow");

        Self::_enforce_max_balance_per_account(env, to, new_to_balance, supply);

        env.storage().persistent().set(&to_key, &new_to_balance);

        env.storage()
            .persistent()
            .extend_ttl(&to_key, ttl_ledgers, ttl_ledgers);

        env.events().publish(
            (symbol_short!("transfer"), from.clone(), to.clone()),
            amount,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _, testutils::Events as _, testutils::Ledger as _, Env, IntoVal,
    };

    // ── Mock compliance nodes ───────────────────────────────────────────
    //
    // The repo had no compliance-node implementation anywhere, so the
    // cross-contract paths in `_check_compliance` / `set_compliance_node`
    // were untestable. These three cover the behaviours that matter:
    // a well-behaved allow/deny node, a node that panics, and a contract
    // that exists but has no `can_trade` at all.

    // Each mock lives in its own module: `#[contractimpl]` emits per-function
    // items whose names are derived from the method name, so two `can_trade`
    // implementations cannot share a module.

    /// A well-behaved node. Allows every trade unless a denied address has
    /// been registered via `deny`, in which case trades touching that address
    /// are rejected.
    pub mod good_node {
        use soroban_sdk::{contract, contractimpl, Address, Env};

        #[contract]
        pub struct MockComplianceNode;

        #[contractimpl]
        impl MockComplianceNode {
            pub fn deny(env: Env, addr: Address) {
                env.storage().instance().set(&addr, &true);
            }

            pub fn can_trade(env: Env, from: Address, to: Address) -> bool {
                let denied = |a: &Address| -> bool {
                    env.storage()
                        .instance()
                        .get::<Address, bool>(a)
                        .unwrap_or(false)
                };
                !denied(&from) && !denied(&to)
            }
        }
    }

    /// A node whose `can_trade` always panics — stands in for an upgraded,
    /// broken, or budget-exhausting node.
    pub mod panicking_node {
        use soroban_sdk::{contract, contractimpl, Address, Env};

        #[contract]
        pub struct PanickingComplianceNode;

        #[contractimpl]
        impl PanickingComplianceNode {
            pub fn can_trade(_env: Env, _from: Address, _to: Address) -> bool {
                panic!("compliance node exploded");
            }
        }
    }

    /// A contract that exists but does not implement `can_trade`.
    pub mod wrong_interface {
        use soroban_sdk::{contract, contractimpl, Env};

        #[contract]
        pub struct WrongInterfaceContract;

        #[contractimpl]
        impl WrongInterfaceContract {
            pub fn unrelated(_env: Env) -> bool {
                true
            }
        }
    }

    /// An allowlist-style node: `can_trade(from, to)` requires *both* sides to
    /// be KYC'd, and it implements `can_issue(to)` so minting is evaluated on
    /// the recipient alone (issue #405). This is the shape that exposed the
    /// original bug — `good_node` above is a deny-list, which never touches
    /// the token contract's own address either way, so it could never have
    /// caught this.
    pub mod allowlist_node {
        use soroban_sdk::{contract, contractimpl, Address, Env};

        #[contract]
        pub struct AllowlistComplianceNode;

        #[contractimpl]
        impl AllowlistComplianceNode {
            pub fn approve(env: Env, addr: Address) {
                env.storage().instance().set(&addr, &true);
            }

            fn is_approved(env: &Env, addr: &Address) -> bool {
                env.storage()
                    .instance()
                    .get::<Address, bool>(addr)
                    .unwrap_or(false)
            }

            pub fn can_trade(env: Env, from: Address, to: Address) -> bool {
                Self::is_approved(&env, &from) && Self::is_approved(&env, &to)
            }

            pub fn can_issue(env: Env, to: Address) -> bool {
                Self::is_approved(&env, &to)
            }
        }
    }

    /// The same allowlist policy, but *without* `can_issue` — models every
    /// compliance node deployed before issue #405, so the token contract's
    /// `can_trade(to, to)` fallback is exercised against a real allowlist
    /// shape rather than only against `good_node`'s deny-list.
    pub mod legacy_allowlist_node {
        use soroban_sdk::{contract, contractimpl, Address, Env};

        #[contract]
        pub struct LegacyAllowlistComplianceNode;

        #[contractimpl]
        impl LegacyAllowlistComplianceNode {
            pub fn approve(env: Env, addr: Address) {
                env.storage().instance().set(&addr, &true);
            }

            pub fn can_trade(env: Env, from: Address, to: Address) -> bool {
                let approved = |a: &Address| -> bool {
                    env.storage()
                        .instance()
                        .get::<Address, bool>(a)
                        .unwrap_or(false)
                };
                approved(&from) && approved(&to)
            }
        }
    }

    use allowlist_node::{AllowlistComplianceNode, AllowlistComplianceNodeClient};
    use good_node::{MockComplianceNode, MockComplianceNodeClient};
    use legacy_allowlist_node::{
        LegacyAllowlistComplianceNode, LegacyAllowlistComplianceNodeClient,
    };
    use panicking_node::PanickingComplianceNode;
    use wrong_interface::WrongInterfaceContract;

    // ── Event topic fixture ─────────────────────────────────────────────
    //
    // The checked-in, single source of truth for every event topic-0 name
    // this contract emits. `docs/events.md` is generated from
    // `docs/events.json`, which must list exactly this set — see issue
    // #340, where the doc silently drifted from the contract (documented
    // 7 events, contract emitted 15, including a `set_admin` event that
    // never existed) and a frontend indexer was then built against the
    // stale doc instead of the contract, dropping whole categories of
    // activity. `scripts/generate_events_doc.py --check` re-derives this
    // same set directly from source and fails CI if it and
    // `docs/events.json` disagree.
    const EXPECTED_TOPICS: [&str; 22] = [
        "approve",
        "authorize",
        "burn",
        "clawback",
        "cncl_adm",
        "freeze",
        "init",
        "mint",
        "pause",
        "prop_adm",
        "rev_auth",
        "revoked",
        "rvk_rvc",
        "set_admin",
        "set_areq",
        "set_cnode",
        "set_max_b",
        "transfer",
        "unfreeze",
        "unpause",
        "upd_uri",
        "upgrade",
    ];

    /// Asserts the set of `symbol_short!("...")` topic-0 literals used in
    /// this file's production code (everything before the test module)
    /// exactly matches `EXPECTED_TOPICS`. This is a static check rather
    /// than a live-invocation one because at least one event (`upgrade`)
    /// can only be reached by an invocation that succeeds, which requires
    /// real WASM bytes to be uploaded first — impractical for a unit
    /// test — so scanning the source for every `.publish(...)` call site
    /// is the only way to cover every event, including ones that are hard
    /// to trigger live.
    #[test]
    fn test_emitted_topics_match_checked_in_fixture() {
        const SOURCE: &str = include_str!("lib.rs");
        let (production_source, _) = SOURCE
            .split_once("#[cfg(test)]")
            .expect("could not locate test module boundary in lib.rs");

        const NEEDLE: &str = "symbol_short!(\"";

        // Every expected topic must actually appear as a symbol_short! literal.
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

        // No symbol_short! literal exists outside the expected set — i.e.
        // nothing new was added without updating the fixture (and
        // docs/events.json / docs/events.md alongside it).
        let mut rest = production_source;
        while let Some(pos) = rest.find(NEEDLE) {
            let after = &rest[pos + NEEDLE.len()..];
            let end = after.find('"').expect("unterminated symbol_short! literal");
            let name = &after[..end];
            assert!(
                EXPECTED_TOPICS.contains(&name),
                "topic {name:?} is emitted by the contract but missing from \
                 EXPECTED_TOPICS (and likely docs/events.json / docs/events.md)"
            );
            rest = &after[end..];
        }
    }

    // ── TTL constant tests ──────────────────────────────────────────────

    #[test]
    fn test_ttl_requests_are_clamped_to_the_network_ceiling() {
        // The archival window is decided by the clamp, not by TTL_LEDGERS:
        // every site asks for TTL_LEDGERS and gets `min(request, max_ttl())`.
        //
        // This deliberately asserts a *relationship* rather than a number.
        // The soroban-sdk test harness reports a `max_entry_ttl` of 6,312,000
        // (its own default, `soroban-sdk/src/env.rs`), while testnet and
        // mainnet both enforce 3,110,400. A test pinning either figure would
        // mislead — and pinning the harness value is exactly how the old
        // ceiling went unnoticed (#398). Note this also means the clamp does
        // *not* bind under test, but does bind on both real networks, where
        // the effective window is about 180 days rather than a year.
        let env = Env::default();
        let contract_id = env.register_contract(None, TokenContract);

        env.as_contract(&contract_id, || {
            let network_max = env.storage().max_ttl();
            let effective = TokenContract::_ttl_ledgers(&env);

            assert_eq!(
                effective,
                TTL_LEDGERS.min(network_max),
                "the effective TTL must be the request clamped to the network \
                 ceiling, never the raw request"
            );
            assert!(
                effective <= network_max,
                "effective TTL ({effective}) exceeds the network ceiling \
                 ({network_max}); extend_ttl would be silently lowered"
            );
        });
    }

    #[test]
    fn test_ttl_ledgers_encodes_a_one_year_request() {
        // Documents intent only: 5s per ledger for 365 days. The window a
        // holder actually gets is shorter wherever the network says so.
        let days = (TTL_LEDGERS as u64 * 5) / (24 * 60 * 60);
        assert_eq!(days, 365);
    }

    fn setup() -> (Env, TokenContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &10_000_000_000_000_i128, // 1M tokens with 7 decimals
            &None,
            &false,
            &false,
            &None,
            &None,
        );

        (env, client, admin, user)
    }

    // ── max_balance_per_account tests ───────────────────────────────────

    #[test]
    fn test_max_balance_per_account_getter_none() {
        let (_, client, _, _) = setup();
        assert_eq!(client.max_balance_per_account(), None);
    }

    #[test]
    fn test_set_max_balance_per_account_enforces_on_transfer() {
        let (_, client, admin, user) = setup();

        client.set_max_balance_per_account(&Some(10u32));

        // total_supply == 1_000_000_0000000; 10% == 100_000_0000000.
        // Transfering the full 10% cap should succeed.
        client.transfer(&admin, &user, &1_000_000_000_000_i128);
        assert_eq!(client.balance(&user), 1_000_000_000_000_i128);
    }

    #[test]
    fn test_set_max_balance_per_account_transfer_exceeds_panics() {
        let (_, client, admin, user) = setup();

        client.set_max_balance_per_account(&Some(10u32));
        client.transfer(&admin, &user, &1_000_000_000_000_i128);

        // one more token should exceed cap.
        assert_eq!(
            client.try_transfer(&admin, &user, &1i128),
            Err(Ok(TokenError::ExceedsMaxBalance.into()))
        );
    }

    #[test]
    fn test_set_max_balance_per_account_enforces_on_mint() {
        let (_, client, _, user) = setup();

        client.set_max_balance_per_account(&Some(10u32));

        // mint up to cap succeeds
        client.mint(&user, &1_000_000_000_000_i128);
        assert_eq!(client.balance(&user), 1_000_000_000_000_i128);
    }

    #[test]
    #[should_panic(expected = "max balance per account exceeded")]
    fn test_set_max_balance_per_account_mint_exceeds_panics() {
        let (_, client, _admin, user) = setup();

        client.set_max_balance_per_account(&Some(10u32));

        // Minting one more base unit should exceed the 10% cap.
        client.mint(&user, &1i128);
    }

    #[test]
    fn test_admin_recipient_exempt_from_max_balance_per_account() {
        let (_, client, admin, user) = setup();

        client.set_max_balance_per_account(&Some(1u32));

        // Transfer to admin should not be blocked even if it would exceed the cap.
        client.mint(&user, &10_000i128);
        client.transfer(&user, &admin, &10_000i128);
    }

    #[test]
    fn test_initialize_and_getters() {
        let (env, client, admin, _) = setup();
        assert_eq!(client.name(), String::from_str(&env, "TestToken"));
        assert_eq!(client.symbol(), String::from_str(&env, "TST"));
        assert_eq!(client.decimals(), 7u32);
        assert_eq!(client.admin(), admin.clone());
        assert_eq!(client.total_supply(), 10_000_000_000_000_i128);
        assert_eq!(client.balance(&admin), 10_000_000_000_000_i128);
    }

    #[test]
    fn test_double_init_panics() {
        let (env, client, admin, _) = setup();
        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "Dup"),
            &String::from_str(&env, "DUP"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
            &None,
        );
    }

    #[test]
    fn test_mint() {
        let (_, client, admin, user) = setup();
        client.mint(&user, &5_000_000_000_i128);
        assert_eq!(client.balance(&user), 5_000_000_000_i128);
        assert_eq!(
            client.total_supply(),
            10_000_000_000_000_i128 + 5_000_000_000_i128
        );
        // admin balance unchanged
        assert_eq!(client.balance(&admin), 10_000_000_000_000_i128);
    }

    #[test]
    fn test_burn() {
        let (_, client, admin, _) = setup();
        client.burn(&admin, &1_000_000_000_i128);
        assert_eq!(
            client.balance(&admin),
            10_000_000_000_000_i128 - 1_000_000_000_i128
        );
        assert_eq!(
            client.total_supply(),
            10_000_000_000_000_i128 - 1_000_000_000_i128
        );
    }

    /// `burn_self` is a non-SEP-41 alias for `burn` (#468). Its only purpose is
    /// to keep the name working for the launchpad's own UI, which calls it
    /// from `buildBurnTransaction` in `frontend/lib/stellar.ts` — so the thing
    /// worth testing is that it still reaches `burn`, not that it has its own
    /// behaviour. `test_burn` and the rest of that suite cover the behaviour
    /// itself; the five near-duplicate `burn_self` suites that used to sit
    /// alongside them were deleted.
    #[test]
    fn test_burn_self_forwards_to_burn() {
        let (env, client, admin, user) = setup();
        client.transfer(&admin, &user, &1_000i128);

        let supply_before = client.total_supply();
        client.burn_self(&user, &400i128);

        assert_eq!(client.balance(&user), 600i128);
        assert_eq!(client.total_supply(), supply_before - 400i128);
        assert_eq!(client.total_burned(), 400i128);

        // Same rejection paths as `burn`, because it is the same code path.
        assert_eq!(
            client.try_burn_self(&user, &0i128),
            Err(Ok(TokenError::InvalidAmount.into()))
        );
        client.freeze_account(&user);
        assert_eq!(
            client.try_burn_self(&user, &100i128),
            Err(Ok(TokenError::Frozen.into()))
        );

        // The forwarded call really is `burn`: the same failure surfaces for
        // both, which is the whole point of collapsing them.
        let other = Address::generate(&env);
        client.transfer(&admin, &other, &10i128);
        assert_eq!(
            client.try_burn_self(&other, &0i128),
            client.try_burn(&other, &0i128)
        );
    }

    #[test]
    fn test_burn_rejects_zero() {
        let (_, client, admin, _) = setup();
        assert_eq!(
            client.try_burn(&admin, &0i128),
            Err(Ok(TokenError::InvalidAmount.into()))
        );
    }

    #[test]
    fn test_mint_batch() {
        let (env, client, _, _) = setup();
        let u1 = Address::generate(&env);
        let u2 = Address::generate(&env);

        let mut to = soroban_sdk::Vec::new(&env);
        to.push_back(u1.clone());
        to.push_back(u2.clone());

        let mut amounts = soroban_sdk::Vec::new(&env);
        amounts.push_back(100i128);
        amounts.push_back(200i128);

        client.mint_batch(&to, &amounts);

        assert_eq!(client.balance(&u1), 100i128);
        assert_eq!(client.balance(&u2), 200i128);
    }

    #[test]
    fn test_mint_batch_len_mismatch() {
        let (env, client, _, _) = setup();
        let u1 = Address::generate(&env);

        let mut to = soroban_sdk::Vec::new(&env);
        to.push_back(u1);

        let mut amounts = soroban_sdk::Vec::new(&env);
        amounts.push_back(100i128);
        amounts.push_back(200i128);

        assert_eq!(
            client.try_mint_batch(&to, &amounts),
            Err(Ok(TokenError::BatchLengthMismatch.into()))
        );
    }

    #[test]
    fn test_mint_batch_exceeds_max_size() {
        let (env, client, _, _) = setup();
        let mut to = soroban_sdk::Vec::new(&env);
        let mut amounts = soroban_sdk::Vec::new(&env);
        for _ in 0..101 {
            let addr = Address::generate(&env);
            to.push_back(addr.clone());
            amounts.push_back(1i128);
        }
        assert_eq!(
            client.try_mint_batch(&to, &amounts),
            Err(Ok(TokenError::BatchTooLarge.into()))
        );
    }

    #[test]
    fn test_total_burned_starts_at_zero() {
        let (_, client, _, _) = setup();
        assert_eq!(client.total_burned(), 0i128);
    }

    #[test]
    fn test_total_burned_after_single_burn() {
        let (_, client, admin, _) = setup();
        client.burn(&admin, &1_000_000_000_i128);
        assert_eq!(client.total_burned(), 1_000_000_000_i128);
    }

    #[test]
    fn test_total_burned_after_two_burns() {
        let (_, client, admin, _) = setup();
        client.burn(&admin, &1_000_000_000_i128);
        client.burn(&admin, &2_500_000_000_i128);
        assert_eq!(client.total_burned(), 3_500_000_000_i128);
    }

    #[test]
    fn test_burn_admin_updates_total_burned() {
        let (_, client, admin, user) = setup();
        client.transfer(&admin, &user, &1_000_000_000_i128);

        client.burn_admin(&user, &400_000_000_i128);

        assert_eq!(client.balance(&user), 600_000_000_i128);
        assert_eq!(client.total_burned(), 400_000_000_i128);
        assert_eq!(
            client.total_supply(),
            10_000_000_000_000_i128 - 400_000_000_i128
        );
    }

    #[test]
    fn test_burn_updates_total_burned_and_total_supply_each_time() {
        let (_, client, admin, _) = setup();

        client.burn(&admin, &1_000_000_000_i128);
        assert_eq!(client.total_burned(), 1_000_000_000_i128);
        assert_eq!(
            client.total_supply(),
            10_000_000_000_000_i128 - 1_000_000_000_i128
        );

        client.burn(&admin, &2_500_000_000_i128);
        assert_eq!(client.total_burned(), 3_500_000_000_i128);
        assert_eq!(
            client.total_supply(),
            10_000_000_000_000_i128 - 3_500_000_000_i128
        );
    }

    #[test]
    fn test_mint_does_not_change_total_burned() {
        let (_, client, admin, user) = setup();

        client.burn(&admin, &1_000_000_000_i128);
        client.mint(&user, &250_000_000_i128);

        assert_eq!(client.total_burned(), 1_000_000_000_i128);
        assert_eq!(
            client.total_supply(),
            10_000_000_000_000_i128 - 1_000_000_000_i128 + 250_000_000_i128
        );
    }

    #[test]
    fn test_burn_insufficient() {
        let (_, client, _, user) = setup();
        assert_eq!(
            client.try_burn(&user, &1i128),
            Err(Ok(TokenError::InsufficientBalance.into()))
        );
    }

    #[test]
    fn test_burn_blocked_when_frozen() {
        let (_, client, admin, user) = setup();
        client.transfer(&admin, &user, &1_000i128);
        client.freeze_account(&user);
        assert_eq!(
            client.try_burn(&user, &500i128),
            Err(Ok(TokenError::Frozen.into()))
        );
    }

    #[test]
    fn test_transfer() {
        let (_, client, admin, user) = setup();
        client.transfer(&admin, &user, &2_500_000_000_i128);
        assert_eq!(
            client.balance(&admin),
            10_000_000_000_000_i128 - 2_500_000_000_i128
        );
        assert_eq!(client.balance(&user), 2_500_000_000_i128);
        // total supply unchanged
        assert_eq!(client.total_supply(), 10_000_000_000_000_i128);
    }

    #[test]
    fn test_transfer_insufficient() {
        let (_, client, _, user) = setup();
        assert_eq!(
            client.try_transfer(&user, &user, &1i128),
            Err(Ok(TokenError::InsufficientBalance.into()))
        );
    }

    #[test]
    fn test_approve_and_transfer_from() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &1_000_000_000_i128, &1000u32);
        assert_eq!(client.allowance(&admin, &spender), 1_000_000_000_i128);

        client.transfer_from(&spender, &admin, &user, &600_000_000_i128);
        assert_eq!(client.allowance(&admin, &spender), 400_000_000_i128);
        assert_eq!(client.balance(&user), 600_000_000_i128);
        assert_eq!(
            client.balance(&admin),
            10_000_000_000_000_i128 - 600_000_000_i128
        );
    }

    // ── `_spend_allowance` — the four branches of the shared allowance path ──
    //
    // `transfer_from` and `burn_from` both delegate here (#467), so the logic
    // is covered once, against the helper, and each entry point's own tests
    // only have to cover where the tokens go. The helper writes to temporary
    // storage belonging to the contract, so it has to be driven through
    // `env.as_contract` to get at the real entries.
    //
    // The two panicking branches are asserted through the public `try_*`
    // surface rather than by calling the helper directly: that is the same
    // panic either way, but it also lets the test prove nothing was written
    // on the way out, which a `should_panic` test cannot check.

    #[test]
    fn test_spend_allowance_leaves_remainder_and_preserves_expiry() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        let contract_id = client.address.clone();

        client.approve(&admin, &spender, &1_000i128, &1_000u32);

        // Sufficient branch: a partial spend rewrites the entry at the
        // reduced amount. SEP-41 requires the expiry to be left alone — a
        // spender must not be able to extend an allowance's life by using it
        // — which `allowance()` alone cannot show, because it reports 0 for
        // an expired entry as well as for a missing one.
        env.as_contract(&contract_id, || {
            TokenContract::_spend_allowance(&env, &spender, &admin, 400i128);
        });

        assert_eq!(client.allowance(&admin, &spender), 600i128);
        env.as_contract(&contract_id, || {
            let stored: Option<AllowanceValue> = env
                .storage()
                .temporary()
                .get(&DataKey::Allowance(admin.clone(), spender.clone()));
            let stored = stored.expect("a partial spend must leave an entry behind");
            assert_eq!(stored.amount, 600i128);
            assert_eq!(stored.expiration_ledger, 1_000u32);
        });
    }

    #[test]
    fn test_spend_allowance_zeroing_out_removes_the_entry() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        let contract_id = client.address.clone();

        client.approve(&admin, &spender, &500i128, &1_000u32);

        // Zeroing branch: spending the allowance down to exactly zero is a
        // revocation, so the entry is deleted rather than left behind as a
        // 0-amount record with a live expiry. `allowance()` returns 0 for
        // both shapes, so the removal itself has to be asserted.
        env.as_contract(&contract_id, || {
            TokenContract::_spend_allowance(&env, &spender, &admin, 500i128);
        });

        assert_eq!(client.allowance(&admin, &spender), 0i128);
        env.as_contract(&contract_id, || {
            assert!(!env
                .storage()
                .temporary()
                .has(&DataKey::Allowance(admin.clone(), spender.clone())));
        });
    }

    #[test]
    fn test_spend_allowance_rejects_insufficient_without_touching_state() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);

        // Insufficient branch: the guard fires before any write, so the
        // allowance survives intact and no tokens move.
        client.approve(&admin, &spender, &10i128, &1_000u32);
        assert_eq!(
            client.try_transfer_from(&spender, &admin, &user, &11i128),
            Err(Ok(TokenError::InsufficientAllowance.into()))
        );
        assert_eq!(client.allowance(&admin, &spender), 10i128);
        assert_eq!(client.balance(&user), 0i128);
        assert_eq!(client.balance(&admin), 10_000_000_000_000_i128);
    }

    #[test]
    fn test_spend_allowance_treats_expired_entry_as_zero() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);

        // Expired branch: SEP-41 says an expired entry reads as a 0 amount
        // allowance, so the same guard as above fires — and it is the reason
        // the helper's `expect("allowance checked above")` is safe to keep
        // even though `stored` is `None` here.
        client.approve(&admin, &spender, &1_000i128, &100u32);
        env.ledger().set_sequence_number(101);

        assert_eq!(client.allowance(&admin, &spender), 0i128);
        assert_eq!(
            client.try_transfer_from(&spender, &admin, &user, &1i128),
            Err(Ok(TokenError::InsufficientAllowance.into()))
        );

        // A large amount fails for the same reason, not by exhausting the
        // allowance arithmetic: the guard is the expiry, and the read is
        // genuinely 0.
        assert_eq!(
            client.try_transfer_from(&spender, &admin, &user, &999_999i128),
            Err(Ok(TokenError::InsufficientAllowance.into()))
        );
    }

    #[test]
    fn test_transfer_from_exceeds_allowance() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &10i128, &1000u32);
        assert_eq!(
            client.try_transfer_from(&spender, &admin, &user, &11i128),
            Err(Ok(TokenError::InsufficientAllowance.into()))
        );
    }

    // ── burn_from tests ─────────────────────────────────────────────────

    #[test]
    fn test_burn_from_happy_path() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &1_000_000_000_i128, &1000u32);
        let supply_before = client.total_supply();

        client.burn_from(&spender, &admin, &600_000_000_i128);

        assert_eq!(client.allowance(&admin, &spender), 400_000_000_i128);
        assert_eq!(
            client.balance(&admin),
            10_000_000_000_000_i128 - 600_000_000_i128
        );
        assert_eq!(client.total_supply(), supply_before - 600_000_000_i128);
        assert_eq!(client.total_burned(), 600_000_000_i128);
    }

    #[test]
    fn test_burn_from_exceeds_allowance() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &10i128, &1000u32);
        // The allowance path is now shared with `transfer_from` (#467), so a
        // short allowance surfaces as the typed `InsufficientAllowance` rather
        // than a panic string. Asserting the code — the same way
        // `transfer_from`'s own test does — is what makes it a contract for
        // clients to depend on, and it is exactly what a release WASM with
        // panic messages stripped would report.
        assert_eq!(
            client.try_burn_from(&spender, &admin, &11i128),
            Err(Ok(TokenError::InsufficientAllowance.into()))
        );
        // The rejected spend must leave both the allowance and the balances
        // untouched, not half-applied.
        assert_eq!(client.allowance(&admin, &spender), 10i128);
        assert_eq!(client.balance(&admin), 10_000_000_000_000_i128);
        assert_eq!(client.total_supply(), 10_000_000_000_000_i128);
        assert_eq!(client.total_burned(), 0i128);
    }

    #[test]
    #[should_panic(expected = "account is frozen")]
    fn test_burn_from_blocked_when_frozen() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);

        client.transfer(&admin, &user, &1_000i128);
        client.approve(&user, &spender, &1_000i128, &1000u32);
        client.freeze_account(&user);

        client.burn_from(&spender, &user, &500i128);
    }

    #[test]
    #[should_panic(expected = "amount must be positive")]
    fn test_burn_from_rejects_zero() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &100i128, &1000u32);
        client.burn_from(&spender, &admin, &0i128);
    }

    #[test]
    #[should_panic(expected = "contract is paused")]
    fn test_burn_from_blocked_when_paused() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &100i128, &1000u32);
        client.pause();
        client.burn_from(&spender, &admin, &10i128);
    }

    #[test]
    fn test_propose_and_accept_admin() {
        let (_, client, _, user) = setup();
        client.propose_admin(&user);
        // Admin has not changed yet
        assert_ne!(client.admin(), user);
        client.accept_admin();
        assert_eq!(client.admin(), user);
    }

    #[test]
    fn test_accept_admin_without_proposal() {
        let (_, client, _, _) = setup();
        assert_eq!(
            client.try_accept_admin(),
            Err(Ok(TokenError::NoPendingAdmin.into()))
        );
    }

    #[test]
    fn test_propose_admin_overwrites_previous() {
        let (env, client, _, user) = setup();
        let other = Address::generate(&env);
        client.propose_admin(&user);
        client.propose_admin(&other);
        client.accept_admin();
        assert_eq!(client.admin(), other);
    }

    #[test]
    #[should_panic(expected = "cannot propose current admin")]
    fn test_propose_admin_rejects_current_admin() {
        let (_, client, admin, _) = setup();
        client.propose_admin(&admin);
    }

    #[test]
    fn test_cancel_admin_proposal_clears_pending_state() {
        let (env, client, _, user) = setup();
        let other = Address::generate(&env);

        client.propose_admin(&user);
        client.propose_admin(&other);
        client.cancel_admin_proposal();

        assert_eq!(client.pending_admin(), None);
    }

    #[test]
    #[should_panic(expected = "pending admin proposal expired")]
    fn test_accept_admin_rejects_expired_proposal() {
        let (env, client, _, user) = setup();

        client.propose_admin(&user);
        env.ledger().set_sequence_number(TTL_LEDGERS + 1);
        client.accept_admin();
    }

    #[test]
    #[should_panic(expected = "contract is paused")]
    fn test_accept_admin_blocked_when_paused() {
        let (_, client, _, user) = setup();

        client.propose_admin(&user);
        client.pause();
        client.accept_admin();
    }

    #[test]
    fn test_pending_admin_getter_reflects_two_step_transfer() {
        let (_, client, _, user) = setup();
        // No transfer in progress yet.
        assert_eq!(client.pending_admin(), None);
        // After proposing, the getter surfaces the pending address.
        client.propose_admin(&user);
        assert_eq!(client.pending_admin(), Some(user.clone()));
        // Accepting clears the pending entry.
        client.accept_admin();
        assert_eq!(client.pending_admin(), None);
    }

    #[test]
    fn test_old_admin_retains_role_until_accepted() {
        let (_, client, admin, user) = setup();
        client.propose_admin(&user);
        // Admin can still mint before acceptance
        client.mint(&user, &1i128);
        assert_eq!(client.admin(), admin);
    }

    // ── Freeze / Unfreeze tests ─────────────────────────────────────────

    #[test]
    fn test_freeze_and_is_frozen() {
        let (_, client, _, user) = setup();
        assert!(!client.is_frozen(&user));
        client.freeze_account(&user);
        assert!(client.is_frozen(&user));
    }

    #[test]
    fn test_frozen_transfer_blocked() {
        let (_, client, admin, user) = setup();
        client.transfer(&admin, &user, &1000i128);
        client.freeze_account(&user);
        assert_eq!(
            client.try_transfer(&user, &admin, &500i128),
            Err(Ok(TokenError::Frozen.into()))
        );
    }

    #[test]
    fn test_frozen_transfer_from_blocked() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);
        // Give user some tokens and approve spender.
        client.transfer(&admin, &user, &1000i128);
        client.approve(&user, &spender, &1000i128, &1000u32);
        // Freeze user, then attempt transfer_from.
        client.freeze_account(&user);
        assert_eq!(
            client.try_transfer_from(&spender, &user, &admin, &500i128),
            Err(Ok(TokenError::Frozen.into()))
        );
    }

    // ── Regression tests for issue #397: freeze must block receive + mint ──
    //
    // Before this fix, `_is_frozen` was only checked on send-side call sites
    // (burn, burn_self, transfer, approve, transfer_from, burn_from). Neither
    // `_transfer`'s recipient nor `_mint`'s recipient was checked, so a frozen
    // account kept receiving inbound transfers and could still be minted into,
    // while the header comment claimed a full "no send or receive" blacklist.
    // These tests pin the receive-side semantics and the deliberate clawback
    // bypass so the contract, the doc comment, and the SecurityCard copy agree.

    #[test]
    #[should_panic(expected = "account is frozen")]
    fn test_frozen_account_cannot_receive_transfer() {
        let (_, client, admin, user) = setup();
        client.freeze_account(&user);
        // Inbound transfer to a frozen account must fail, not silently credit.
        client.transfer(&admin, &user, &1_000i128);
    }

    #[test]
    #[should_panic(expected = "account is frozen")]
    fn test_frozen_account_cannot_receive_transfer_from() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);
        client.approve(&admin, &spender, &1_000i128, &1000u32);
        // Freeze the *recipient*; the sender/allowance side is fine.
        client.freeze_account(&user);
        client.transfer_from(&spender, &admin, &user, &500i128);
    }

    #[test]
    #[should_panic(expected = "account is frozen")]
    fn test_frozen_account_cannot_be_minted_into() {
        let (_, client, _, user) = setup();
        client.freeze_account(&user);
        client.mint(&user, &1_000i128);
    }

    #[test]
    #[should_panic(expected = "account is frozen")]
    fn test_frozen_account_cannot_be_minted_into_via_batch() {
        let (env, client, _, user) = setup();
        let ok = Address::generate(&env);
        client.freeze_account(&user);

        let mut to = soroban_sdk::Vec::new(&env);
        to.push_back(ok);
        to.push_back(user);

        let mut amounts = soroban_sdk::Vec::new(&env);
        amounts.push_back(100i128);
        amounts.push_back(200i128);

        // One frozen recipient must revert the whole batch.
        client.mint_batch(&to, &amounts);
    }

    #[test]
    fn test_clawback_still_recovers_from_frozen_account() {
        let (_, client, admin, user) = setup();
        // Fund the user before freezing (transfer to an unfrozen account is fine).
        client.transfer(&admin, &user, &1_000i128);
        let admin_before = client.balance(&admin);

        client.freeze_account(&user);
        // Clawback must bypass the freeze on both sides and pull funds to admin.
        client.clawback(&user, &1_000i128);

        assert_eq!(client.balance(&user), 0i128);
        assert_eq!(client.balance(&admin), admin_before + 1_000i128);
    }

    #[test]
    fn test_unfreeze_restores_receive() {
        let (_, client, admin, user) = setup();
        client.freeze_account(&user);
        client.unfreeze_account(&user);
        // Once unfrozen, the account can receive again.
        client.transfer(&admin, &user, &1_000i128);
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_frozen_entry_survives_the_minimum_persistent_ttl() {
        let (env, client, _, user) = setup();
        let min_ttl = env.ledger().get().min_persistent_entry_ttl;

        client.freeze_account(&user);
        // A bare persistent `set` would archive at this point. The freeze
        // policy must instead retain the clamped long-lived TTL.
        env.ledger().set_sequence_number(min_ttl + 1);

        assert!(client.is_frozen(&user));
    }

    // ── Revoke admin / lock tests ───────────────────────────────────────

    #[test]
    fn test_revoke_admin_sets_locked_flag() {
        let (_, client, _, _) = setup();
        assert!(!client.is_locked());
        client.revoke_admin();
        assert!(client.is_locked());
    }

    #[test]
    fn test_revoke_admin_requires_compliance_node_to_be_cleared() {
        let (env, client, admin, user) = setup();
        let node = register_good_node(&env);
        client.set_compliance_node(&Some(node));

        assert_eq!(
            client.try_revoke_admin(),
            Err(Ok(TokenError::ComplianceNodeMustBeCleared.into()))
        );

        client.set_compliance_node(&None);
        client.revoke_admin();
        client.transfer(&admin, &user, &1_000i128);
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_admin_getter_after_revoke_panics() {
        let (_, client, _, _) = setup();
        client.revoke_admin();
        assert_eq!(client.try_admin(), Err(Ok(TokenError::Locked.into())));
    }

    #[test]
    fn test_admin_if_any_distinguishes_active_and_revoked_admin() {
        let (_, client, admin, _) = setup();
        assert_eq!(client.admin_if_any(), Some(admin));
        client.revoke_admin();
        assert_eq!(client.admin_if_any(), None);
    }

    #[test]
    fn test_mint_after_revoke_panics() {
        let (_, client, _, user) = setup();
        client.revoke_admin();
        assert_eq!(
            client.try_mint(&user, &1i128),
            Err(Ok(TokenError::Locked.into()))
        );
    }

    // ── Regression test for issue #322: initialize() re-callable after revoke_admin ──

    #[test]
    fn test_initialize_after_revoke_admin_panics() {
        let (env, client, admin, _) = setup();
        client.revoke_admin();
        // Admin storage entry is gone, but Initialized must still block re-init.
        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "Attacker"),
            &String::from_str(&env, "EVL"),
            &1_000_000i128,
            &None,
            &false,
            &false,
            &None,
            &None,
        );
    }

    #[test]
    fn test_burn_admin_after_revoke_panics() {
        let (_, client, admin, _) = setup();
        client.revoke_admin();
        assert_eq!(
            client.try_burn_admin(&admin, &1i128),
            Err(Ok(TokenError::Locked.into()))
        );
    }

    #[test]
    fn test_propose_admin_after_revoke_panics() {
        let (env, client, _, _) = setup();
        let other = Address::generate(&env);
        client.revoke_admin();
        assert_eq!(
            client.try_propose_admin(&other),
            Err(Ok(TokenError::Locked.into()))
        );
    }

    #[test]
    fn test_freeze_after_revoke_panics() {
        let (_, client, _, user) = setup();
        client.revoke_admin();
        assert_eq!(
            client.try_freeze_account(&user),
            Err(Ok(TokenError::Locked.into()))
        );
    }

    #[test]
    fn test_holder_actions_still_work_after_revoke() {
        let (env, client, admin, user) = setup();
        // Move some tokens to the user before locking.
        client.transfer(&admin, &user, &1_000i128);

        // Set a max-balance cap — then revoke (which must deactivate the cap).
        client.set_max_balance_per_account(&Some(50u32));
        client.revoke_admin();

        // Transfers, approvals and self-burn must still work.
        client.transfer(&user, &admin, &200i128);
        assert_eq!(client.balance(&user), 800i128);

        // User-to-user transfer also exercises the cap path with no admin present.
        let user2 = Address::generate(&env);
        client.transfer(&user, &user2, &100i128);
        assert_eq!(client.balance(&user), 700i128);
        assert_eq!(client.balance(&user2), 100i128);

        client.approve(&user, &user2, &50i128, &100);
        client.transfer_from(&user2, &user, &admin, &50i128);
        assert_eq!(client.balance(&user), 650i128);

        client.burn_self(&user, &100i128);
        assert_eq!(client.balance(&user), 550i128);
    }

    #[test]
    fn test_non_admin_can_exceed_cap_after_revoke() {
        let (_, client, admin, user) = setup();

        // Enable a whale cap and move some value to a non-admin holder.
        client.set_max_balance_per_account(&Some(10u32));
        client.transfer(&admin, &user, &100_000i128);

        // While the cap is active, exceeding 10% of total supply is rejected.
        // total_supply == 1_000_000_0000000, 10% == 100_000_0000000.
        assert_eq!(
            client.try_transfer(&admin, &user, &100_000_0000000i128),
            Err(Ok(TokenError::ExceedsMaxBalance.into()))
        );

        // After revoke_admin the cap is deactivated along with the admin role.
        client.revoke_admin();
        assert!(client.is_locked());

        // The getter goes honest: it no longer reports an enforced limit.
        assert_eq!(client.max_balance_per_account(), None);

        // A non-admin can now accumulate past the former cap without panic.
        client.transfer(&admin, &user, &100_000_0000000i128);
        assert_eq!(client.balance(&user), 100_000_0100000i128);
    }

    #[test]
    fn test_unfreeze_restores_transfer() {
        let (_, client, admin, user) = setup();
        client.transfer(&admin, &user, &1000i128);
        client.freeze_account(&user);
        assert!(client.is_frozen(&user));
        client.unfreeze_account(&user);
        assert!(!client.is_frozen(&user));
        // Transfer should now succeed.
        client.transfer(&user, &admin, &500i128);
        assert_eq!(client.balance(&user), 500i128);
    }

    #[test]
    fn test_non_admin_cannot_freeze() {
        let env = Env::default();
        // Do NOT mock all auths — we want real auth checks.
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
            &None,
        );

        // Remove mock — only user will auth, not admin.
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &user,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "freeze_account",
                args: (&user,).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        // Should fail — user is not admin.
        assert!(client.try_freeze_account(&user).is_err());
    }

    // ── Pause / Unpause tests ───────────────────────────────────────────

    #[test]
    fn test_pause_and_is_paused() {
        let (_, client, _, _) = setup();
        assert!(!client.is_paused());
        client.pause();
        assert!(client.is_paused());
        client.unpause();
        assert!(!client.is_paused());
    }

    #[test]
    fn test_paused_mint_blocked() {
        let (_, client, _, user) = setup();
        client.pause();
        assert_eq!(
            client.try_mint(&user, &1000i128),
            Err(Ok(TokenError::Paused.into()))
        );
    }

    #[test]
    fn test_paused_burn_blocked() {
        let (_, client, admin, _) = setup();
        client.pause();
        assert_eq!(
            client.try_burn(&admin, &1000i128),
            Err(Ok(TokenError::Paused.into()))
        );
    }

    #[test]
    fn test_paused_transfer_blocked() {
        let (_, client, admin, user) = setup();
        client.pause();
        assert_eq!(
            client.try_transfer(&admin, &user, &1000i128),
            Err(Ok(TokenError::Paused.into()))
        );
    }

    #[test]
    fn test_paused_transfer_from_blocked() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);
        client.approve(&admin, &spender, &1000i128, &1000u32);
        client.pause();
        assert_eq!(
            client.try_transfer_from(&spender, &admin, &user, &500i128),
            Err(Ok(TokenError::Paused.into()))
        );
    }

    #[test]
    fn test_unpause_restores_all_operations() {
        let (_, client, admin, user) = setup();
        client.pause();
        client.unpause();

        // All should succeed now
        client.mint(&user, &1000i128);
        client.burn(&user, &500i128);
        client.transfer(&admin, &user, &1000i128);

        assert_eq!(client.balance(&user), 1500i128);
    }

    #[test]
    fn test_read_only_works_while_paused() {
        let (_, client, admin, _) = setup();
        client.pause();

        // Read-only getters should still work
        assert_eq!(client.total_supply(), 10_000_000_000_000_i128);
        assert_eq!(client.balance(&admin), 10_000_000_000_000_i128);
        assert!(client.is_paused());
    }

    #[test]
    fn test_non_admin_cannot_pause() {
        let env = Env::default();
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
            &None,
        );

        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &user,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "pause",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_pause().is_err());
    }

    // ── max_supply tests ────────────────────────────────────────────────
    fn setup_with_cap() -> (Env, TokenContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "CappedToken"),
            &String::from_str(&env, "CAP"),
            &5_000_000_000_i128,
            &Some(10_000_000_000_i128),
            &false,
            &false,
            &None,
            &None,
        );

        (env, client, admin, user)
    }

    #[test]
    fn test_max_supply_getter_none() {
        let (_, client, _, _) = setup();
        assert_eq!(client.max_supply(), None);
    }

    #[test]
    fn test_max_supply_getter_some() {
        let (_, client, _, _) = setup_with_cap();
        assert_eq!(client.max_supply(), Some(10_000_000_000_i128));
    }

    #[test]
    fn test_mint_within_max_supply() {
        let (_, client, _, user) = setup_with_cap();
        client.mint(&user, &5_000_000_000_i128);
        assert_eq!(client.total_supply(), 10_000_000_000_i128);
    }

    #[test]
    fn test_mint_exceeds_max_supply() {
        let (_, client, _, user) = setup_with_cap();
        assert_eq!(
            client.try_mint(&user, &500_0000001i128),
            Err(Ok(TokenError::ExceedsMaxSupply.into()))
        );
    }

    #[test]
    fn test_mint_exact_max_supply() {
        let (_, client, _, user) = setup_with_cap();
        // Mint exactly to the cap boundary
        client.mint(&user, &5_000_000_000_i128);
        assert_eq!(client.total_supply(), 10_000_000_000_i128);
        assert_eq!(client.max_supply(), Some(10_000_000_000_i128));
    }

    #[test]
    fn test_initial_supply_exceeds_max_supply() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "Bad"),
            &String::from_str(&env, "BAD"),
            &20_000_000_000_i128,
            &Some(10_000_000_000_i128),
            &false,
            &false,
            &None,
            &None,
        );
    }

    #[test]
    fn test_update_and_get_contract_uri() {
        let (env, client, _, _) = setup();
        let uri = String::from_str(&env, "https://example.com/token-metadata.json");
        client.update_contract_uri(&uri);
        assert_eq!(client.contract_uri(), Some(uri));
    }

    #[test]
    fn test_update_contract_uri_overwrites() {
        let (env, client, _, _) = setup();
        let uri_a = String::from_str(&env, "https://example.com/a.json");
        let uri_b = String::from_str(&env, "https://example.com/b.json");
        client.update_contract_uri(&uri_a);
        client.update_contract_uri(&uri_b);
        assert_eq!(client.contract_uri(), Some(uri_b));
    }

    #[test]
    fn test_contract_uri_not_set_returns_none() {
        let (_, client, _, _) = setup();
        assert_eq!(client.contract_uri(), None);
    }

    #[test]
    fn test_initialize_sets_contract_uri() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let uri = String::from_str(&env, "https://example.com/token-metadata.json");

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
            &Some(uri.clone()),
        );

        assert_eq!(client.contract_uri(), Some(uri));
    }

    #[test]
    fn test_update_contract_uri_accepts_https_and_ipfs() {
        let (env, client, _, _) = setup();
        for raw in [
            "https://example.com/token-metadata.json",
            "HTTPS://example.com/meta.json",
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi/metadata.json",
        ] {
            let uri = String::from_str(&env, raw);
            client.update_contract_uri(&uri);
            assert_eq!(client.contract_uri(), Some(uri));
        }
    }

    #[test]
    fn test_update_contract_uri_rejects_unsafe_uris() {
        let (env, client, _, _) = setup();
        for raw in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "http://example.com/meta.json",
            "https:example.com",
            "https://",
            " https://example.com",
            "https://example.com/a b",
            "https://example.com/\n",
            "",
        ] {
            assert_eq!(
                client.try_update_contract_uri(&String::from_str(&env, raw)),
                Err(Ok(TokenError::InvalidContractUri.into())),
                "expected {raw:?} to be rejected"
            );
        }
        assert_eq!(client.contract_uri(), None);
    }

    /// An `https://` URI of exactly `len` bytes.
    fn https_uri_of_len(env: &Env, len: usize) -> String {
        let mut buf = [b'a'; 2 * MAX_CONTRACT_URI_LEN as usize];
        let prefix = b"https://example.com/";
        buf[..prefix.len()].copy_from_slice(prefix);
        String::from_bytes(env, &buf[..len])
    }

    #[test]
    fn test_update_contract_uri_length_bound() {
        let (env, client, _, _) = setup();
        let max = MAX_CONTRACT_URI_LEN as usize;

        let at_limit = https_uri_of_len(&env, max);
        client.update_contract_uri(&at_limit);
        assert_eq!(client.contract_uri(), Some(at_limit.clone()));

        assert_eq!(
            client.try_update_contract_uri(&https_uri_of_len(&env, max + 1)),
            Err(Ok(TokenError::InvalidContractUri.into()))
        );
        assert_eq!(client.contract_uri(), Some(at_limit));
    }

    #[test]
    fn test_initialize_rejects_unsafe_contract_uri() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        let result = client.try_initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
            &Some(String::from_str(&env, "javascript:alert(1)")),
        );

        assert_eq!(result, Err(Ok(TokenError::InvalidContractUri.into())));
    }
    // ── Upgrade tests ───────────────────────────────────────────────────

    #[test]
    fn test_upgrade_rejects_zero_hash() {
        let (env, client, _, _) = setup();
        let zero_hash = BytesN::from_array(&env, &[0; 32]);
        assert_eq!(
            client.try_upgrade(&zero_hash),
            Err(Ok(TokenError::InvalidWasmHash.into()))
        );
    }

    #[test]
    fn test_non_admin_cannot_upgrade() {
        let env = Env::default();
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
            &None,
        );

        let non_zero_hash = BytesN::from_array(&env, &[1; 32]);
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &user,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "upgrade",
                args: (non_zero_hash.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(client.try_upgrade(&non_zero_hash).is_err());
    }

    // ── Authorization flag tests ────────────────────────────────────────

    fn setup_with_auth_required() -> (Env, TokenContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "RegToken"),
            &String::from_str(&env, "REG"),
            &10_000_000_000_i128,
            &None,
            &true,
            &true,
            &None,
            &None,
        );

        (env, client, admin, user)
    }

    #[test]
    fn test_authorization_required_flag_stored() {
        let (_, client, _, _) = setup_with_auth_required();
        assert!(client.authorization_required());
        assert!(client.authorization_revocable());
    }

    #[test]
    fn test_authorization_required_is_masked_after_revoke() {
        let (_, client, admin, user) = setup_with_auth_required();
        client.revoke_admin();
        assert!(!client.authorization_required());
        client.transfer(&admin, &user, &1_000i128);
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_authorization_flags_false_by_default() {
        let (_, client, _, _) = setup();
        assert!(!client.authorization_required());
        assert!(!client.authorization_revocable());
    }

    #[test]
    fn test_admin_auto_authorized_on_init() {
        let (_, client, admin, _) = setup_with_auth_required();
        assert!(client.is_authorized(&admin));
    }

    #[test]
    fn test_unauthorized_holder_not_authorized() {
        let (_, client, _, user) = setup_with_auth_required();
        assert!(!client.is_authorized(&user));
    }

    #[test]
    fn test_transfer_to_unauthorized_blocked() {
        let (_, client, admin, user) = setup_with_auth_required();
        assert_eq!(
            client.try_transfer(&admin, &user, &1_000_000_000_i128),
            Err(Ok(TokenError::NotAuthorizedHolder.into()))
        );
    }

    #[test]
    fn test_transfer_to_authorized_succeeds() {
        let (_, client, admin, user) = setup_with_auth_required();
        client.authorize_holder(&user);
        assert!(client.is_authorized(&user));
        client.transfer(&admin, &user, &1_000_000_000_i128);
        assert_eq!(client.balance(&user), 1_000_000_000_i128);
    }

    #[test]
    fn test_authorized_holder_survives_the_minimum_persistent_ttl() {
        let (env, client, _, user) = setup_with_auth_required();
        let min_ttl = env.ledger().get().min_persistent_entry_ttl;

        client.authorize_holder(&user);
        // A newly written persistent allowlist entry only gets this short
        // lifetime unless authorize_holder extends it explicitly.
        env.ledger().set_sequence_number(min_ttl + 1);

        assert!(client.is_authorized(&user));
    }

    #[test]
    fn test_revoke_authorization_blocks_transfer() {
        let (_, client, admin, user) = setup_with_auth_required();
        client.authorize_holder(&user);
        client.transfer(&admin, &user, &1_000_000_000_i128);
        client.revoke_authorization(&user);
        assert!(!client.is_authorized(&user));
    }

    // ── set_authorization_required / renounce_authorization_revocable (#404) ─

    #[test]
    fn test_set_authorization_required_enables_gate_after_deploy() {
        // Deployed without gating — the deploy-time choice must not be
        // permanent (issue #404).
        let (_, client, admin, user) = setup();
        assert!(!client.authorization_required());

        client.set_authorization_required(&true);
        assert!(client.authorization_required());

        // The gate now applies to holders who were never explicitly
        // authorized, exactly as if it had been enabled at deploy time.
        let err = client.try_transfer(&admin, &user, &100_0000000i128);
        assert!(err.is_err());
    }

    #[test]
    fn test_set_authorization_required_disables_gate_after_deploy() {
        let (_, client, admin, user) = setup_with_auth_required();
        client.set_authorization_required(&false);
        assert!(!client.authorization_required());

        // No explicit authorize_holder call — the gate being off means
        // every holder is treated as authorized.
        client.transfer(&admin, &user, &100_0000000i128);
        assert_eq!(client.balance(&user), 100_0000000i128);
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_set_authorization_required_after_revoke_panics() {
        let (_, client, _, _) = setup();
        client.revoke_admin();
        client.set_authorization_required(&true);
    }

    #[test]
    fn test_renounce_authorization_revocable_disables_future_revokes() {
        let (_, client, _, user) = setup_with_auth_required();
        assert!(client.authorization_revocable());

        client.renounce_authorization_revocable();
        assert!(!client.authorization_revocable());

        let err = client.try_revoke_authorization(&user);
        assert!(err.is_err());
    }

    #[test]
    #[should_panic(expected = "authorization revocation is already permanently disabled")]
    fn test_renounce_authorization_revocable_is_one_way() {
        // Deployed non-revocable — there is deliberately no path back to
        // `true`, matching the issue's "one-way only" requirement. Renouncing
        // an already-disabled flag is rejected rather than silently
        // succeeding, so a caller cannot mistake it for having just done
        // something.
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "ReqOnly"),
            &String::from_str(&env, "ROT"),
            &0i128,
            &None,
            &true,
            &false, // revocable = false from deploy
            &None,
            &None,
        );

        client.renounce_authorization_revocable();
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_renounce_authorization_revocable_after_revoke_panics() {
        let (_, client, _, _) = setup_with_auth_required();
        client.revoke_admin();
        client.renounce_authorization_revocable();
    }

    #[test]
    fn test_revoke_fails_when_not_revocable() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "ReqOnly"),
            &String::from_str(&env, "ROT"),
            &0i128,
            &None,
            &true,
            &false, // revocable = false
            &None,
            &None,
        );

        client.authorize_holder(&user);
        assert_eq!(
            client.try_revoke_authorization(&user),
            Err(Ok(TokenError::NotRevocable.into()))
        );
    }

    #[test]
    fn test_mint_to_unauthorized_blocked() {
        let (_, client, _, user) = setup_with_auth_required();
        assert_eq!(
            client.try_mint(&user, &1000i128),
            Err(Ok(TokenError::NotAuthorizedHolder.into()))
        );
    }

    #[test]
    fn test_mint_to_authorized_succeeds() {
        let (_, client, _, user) = setup_with_auth_required();
        client.authorize_holder(&user);
        client.mint(&user, &1000i128);
        assert_eq!(client.balance(&user), 1000i128);
    }

    // ── approve expiration tests ────────────────────────────────────────

    #[test]
    fn test_approve_expired_ledger_panics() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        // Ledger sequence is 0 by default; expiration_ledger = 0 is NOT in the future.
        assert_eq!(
            client.try_approve(&admin, &spender, &100i128, &0u32),
            Err(Ok(TokenError::InvalidLedgerRange.into()))
        );
    }

    #[test]
    fn test_approve_respects_expiration_ledger() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        // Supply a valid future expiration; the allowance should be stored correctly.
        client.approve(&admin, &spender, &500i128, &100u32);
        assert_eq!(client.allowance(&admin, &spender), 500i128);
    }

    // ── Regression tests for issue #326: allowance expiry semantics ────

    #[test]
    fn test_allowance_returns_zero_after_expiry() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &100i128, &100u32);
        assert_eq!(client.allowance(&admin, &spender), 100i128);

        // Advance past expiration_ledger.
        env.ledger().set_sequence_number(200);

        // Must read back as 0, not panic with an archived-entry error.
        assert_eq!(client.allowance(&admin, &spender), 0i128);
    }

    #[test]
    fn test_transfer_from_after_expiry_reverts_cleanly() {
        let (env, client, admin, user) = setup();
        let spender = Address::generate(&env);

        client.approve(&admin, &spender, &100i128, &100u32);
        env.ledger().set_sequence_number(200);

        // Must revert with the standard insufficient allowance error
        assert_eq!(
            client.try_transfer_from(&spender, &admin, &user, &1i128),
            Err(Ok(TokenError::InsufficientAllowance.into()))
        );
    }

    #[test]
    fn test_approve_zero_allows_revocation() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        // First set a valid allowance
        client.approve(&admin, &spender, &100i128, &100u32);
        assert_eq!(client.allowance(&admin, &spender), 100i128);
        // Now revoke it with 0 amount and 0 expiration
        client.approve(&admin, &spender, &0i128, &0u32);
        assert_eq!(client.allowance(&admin, &spender), 0i128);
    }

    #[test]
    fn test_approve_clamped_ttl() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        // Approve with u32::MAX expiration_ledger (very large, exceeds network max_ttl)
        // Clamping should prevent panic.
        client.approve(&admin, &spender, &100i128, &u32::MAX);
        assert_eq!(client.allowance(&admin, &spender), 100i128);
    }

    #[test]
    #[should_panic(expected = "contract is paused")]
    fn test_approve_blocked_when_paused() {
        let (env, client, admin, _) = setup();
        let spender = Address::generate(&env);
        client.pause();
        client.approve(&admin, &spender, &100i128, &1000u32);
    }

    #[test]
    #[should_panic(expected = "account is frozen")]
    fn test_approve_blocked_when_frozen() {
        let (env, client, _, user) = setup();
        let spender = Address::generate(&env);
        client.freeze_account(&user);
        client.approve(&user, &spender, &100i128, &1000u32);
    }

    #[test]
    fn test_pause_unpause_events() {
        let (env, client, _admin, _) = setup();

        client.pause();
        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("pause"),).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );

        client.unpause();
        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("unpause"),).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );
    }

    // ── Compliance node containment (#327) ──────────────────────────────

    fn register_good_node(env: &Env) -> Address {
        env.register_contract(None, MockComplianceNode)
    }

    #[test]
    fn test_set_and_remove_compliance_node() {
        let (env, client, _, _) = setup();
        let node = register_good_node(&env);

        client.set_compliance_node(&Some(node.clone()));
        assert_eq!(client.compliance_node(), Some(node.clone()));

        client.set_compliance_node(&None);
        assert_eq!(client.compliance_node(), None);
    }

    #[test]
    fn test_compliance_node_is_masked_after_revoke() {
        let (env, client, admin, user) = setup();
        let node = register_good_node(&env);
        MockComplianceNodeClient::new(&env, &node).deny(&user);
        client.set_compliance_node(&Some(node));
        client.revoke_admin();
        assert_eq!(client.compliance_node(), None);
        client.transfer(&admin, &user, &1_000i128);
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_set_compliance_node_rejects_non_contract_address() {
        let (env, client, _, _) = setup();
        // A plain account address is not a contract, so the probe fails and the
        // address is refused instead of silently bricking every transfer.
        assert_eq!(
            client.try_set_compliance_node(&Some(Address::generate(&env))),
            Err(Ok(TokenError::InvalidComplianceNode.into()))
        );
    }

    #[test]
    fn test_set_compliance_node_rejects_contract_without_can_trade() {
        let (env, client, _, _) = setup();
        let wrong = env.register_contract(None, WrongInterfaceContract);
        assert_eq!(
            client.try_set_compliance_node(&Some(wrong)),
            Err(Ok(TokenError::InvalidComplianceNode.into()))
        );
    }

    #[test]
    fn test_set_compliance_node_rejects_the_token_itself() {
        let (_, client, _, _) = setup();
        // The simplest form of the bricking mistake: pointing the token at
        // itself. The probe re-enters and fails, so it never gets stored.
        let own = client.address.clone();
        assert_eq!(
            client.try_set_compliance_node(&Some(own)),
            Err(Ok(TokenError::InvalidComplianceNode.into()))
        );
    }

    #[test]
    fn test_set_compliance_node_rejects_panicking_node() {
        let (env, client, _, _) = setup();
        let node = env.register_contract(None, PanickingComplianceNode);
        assert_eq!(
            client.try_set_compliance_node(&Some(node)),
            Err(Ok(TokenError::InvalidComplianceNode.into()))
        );
    }

    #[test]
    fn test_compliance_node_allows_and_blocks_transfers() {
        let (env, client, admin, user) = setup();
        let node = register_good_node(&env);
        let node_client = MockComplianceNodeClient::new(&env, &node);

        client.set_compliance_node(&Some(node.clone()));
        client.transfer(&admin, &user, &1_000i128);
        assert_eq!(client.balance(&user), 1_000i128);

        node_client.deny(&user);
        let err = client.try_transfer(&admin, &user, &1_000i128);
        assert!(err.is_err());
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_broken_compliance_node_is_contained_and_recoverable() {
        let (env, client, admin, user) = setup();

        // Register a good node, then swap its WASM out from under the token by
        // pointing at a fresh address that stopped behaving. Simulating the
        // real hazard: the stored node was valid at set time and is not now.
        let node = register_good_node(&env);
        client.set_compliance_node(&Some(node.clone()));

        // Re-register the same contract id with the panicking implementation.
        env.register_contract(&node, PanickingComplianceNode);

        // Transfers fail closed with a typed error, not a raw host error.
        let err = client.try_transfer(&admin, &user, &1_000i128);
        assert_eq!(err, Err(Ok(TokenError::ComplianceNodeUnavailable.into())));

        // ...and the admin can always recover by clearing the node.
        client.set_compliance_node(&None);
        assert_eq!(client.compliance_node(), None);
        client.transfer(&admin, &user, &1_000i128);
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_compliance_node_rejection_is_typed() {
        let (env, client, admin, user) = setup();
        let node = register_good_node(&env);
        MockComplianceNodeClient::new(&env, &node).deny(&user);
        client.set_compliance_node(&Some(node));

        assert_eq!(
            client.try_transfer(&admin, &user, &1_000i128),
            Err(Ok(TokenError::ComplianceRejected.into()))
        );
    }

    #[test]
    fn test_compliance_node_gates_mint() {
        let (env, client, _admin, user) = setup();
        let node = register_good_node(&env);
        let node_client = MockComplianceNodeClient::new(&env, &node);
        client.set_compliance_node(&Some(node));

        client.mint(&user, &500i128);
        assert_eq!(client.balance(&user), 500i128);

        // A rejected recipient can no longer be minted into.
        node_client.deny(&user);
        assert_eq!(
            client.try_mint(&user, &500i128),
            Err(Ok(TokenError::ComplianceRejected.into()))
        );
        assert_eq!(client.balance(&user), 500i128);
    }

    #[test]
    fn test_allowlist_node_can_issue_gates_mint_by_recipient_only() {
        // Regression test for issue #405: an allowlist-style node whose
        // `can_trade(from, to)` requires *both* sides to be approved would,
        // before the fix, always reject every mint — the token contract's own
        // address stood in for `from` and was never on the allowlist. With
        // `can_issue(to)` the node is asked about the recipient alone.
        let (env, client, _admin, user) = setup();
        let node = env.register_contract(None, AllowlistComplianceNode);
        let node_client = AllowlistComplianceNodeClient::new(&env, &node);
        client.set_compliance_node(&Some(node));

        // Not yet approved: mint is rejected, not silently allowed.
        assert_eq!(
            client.try_mint(&user, &500i128),
            Err(Ok(TokenError::ComplianceRejected.into()))
        );
        assert_eq!(client.balance(&user), 0i128);

        // Approve the recipient only — the token contract's own address is
        // still never on the allowlist, and that must no longer matter.
        node_client.approve(&user);
        client.mint(&user, &500i128);
        assert_eq!(client.balance(&user), 500i128);
    }

    #[test]
    fn test_allowlist_node_can_issue_gates_mint_batch_by_recipient_only() {
        let (env, client, _admin, _user) = setup();
        let node = env.register_contract(None, AllowlistComplianceNode);
        let node_client = AllowlistComplianceNodeClient::new(&env, &node);
        client.set_compliance_node(&Some(node));

        let recipient = Address::generate(&env);
        node_client.approve(&recipient);

        let to = soroban_sdk::vec![&env, recipient.clone()];
        let amounts = soroban_sdk::vec![&env, 100i128];
        client.mint_batch(&to, &amounts);
        assert_eq!(client.balance(&recipient), 100i128);
    }

    #[test]
    fn test_legacy_allowlist_node_without_can_issue_falls_back_to_can_trade_to_to() {
        // The node predates issue #405 and has no `can_issue` export at all —
        // `try_can_issue` must fail at the invocation level (function not
        // found), and the token contract must fall back to `can_trade(to, to)`
        // rather than treating the missing export as ComplianceNodeUnavailable.
        let (env, client, _admin, user) = setup();
        let node = env.register_contract(None, LegacyAllowlistComplianceNode);
        let node_client = LegacyAllowlistComplianceNodeClient::new(&env, &node);
        client.set_compliance_node(&Some(node));

        // Before the fix this would panic with ComplianceNodeUnavailable (or,
        // depending on interpretation, ComplianceRejected) for *every*
        // recipient, approved or not, because `from` (the token contract's
        // own address) can never be on the allowlist.
        assert_eq!(
            client.try_mint(&user, &500i128),
            Err(Ok(TokenError::ComplianceRejected.into()))
        );

        node_client.approve(&user);
        client.mint(&user, &500i128);
        assert_eq!(client.balance(&user), 500i128);
    }

    #[test]
    fn test_compliance_node_gates_clawback() {
        let (env, client, admin, user) = setup();
        client.transfer(&admin, &user, &1_000i128);

        let node = register_good_node(&env);
        let node_client = MockComplianceNodeClient::new(&env, &node);
        client.set_compliance_node(&Some(node));

        node_client.deny(&user);
        assert_eq!(
            client.try_clawback(&user, &1_000i128),
            Err(Ok(TokenError::ComplianceRejected.into()))
        );
        assert_eq!(client.balance(&user), 1_000i128);
    }

    #[test]
    fn test_compliance_node_does_not_gate_burn() {
        let (env, client, admin, user) = setup();
        client.transfer(&admin, &user, &1_000i128);

        let node = register_good_node(&env);
        let node_client = MockComplianceNodeClient::new(&env, &node);
        client.set_compliance_node(&Some(node));
        node_client.deny(&user);

        // Documented policy: burns are out of scope, so a denied holder can
        // still destroy their own tokens rather than having them trapped.
        client.burn(&user, &400i128);
        assert_eq!(client.balance(&user), 600i128);
    }

    #[test]
    fn test_freeze_unfreeze_events() {
        let (env, client, _admin, user) = setup();

        client.freeze_account(&user);
        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);

        // Verify freeze event
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("freeze"), user.clone()).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );

        client.unfreeze_account(&user);
        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);

        // Verify unfreeze event
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("unfreeze"), user.clone()).into_val(&env),
                    ().into_val(&env)
                )
            ]
        );
    }

    // ── launch_seal tests (#493) ────────────────────────────────────────

    #[test]
    fn test_launch_seal_records_ledger_and_supply() {
        use soroban_sdk::testutils::Ledger as _;
        let (env, client, _, _) = setup();
        assert!(!client.sealed());
        assert_eq!(client.launch_seal_info(), None);

        env.ledger().set_sequence_number(1234);
        let commitment = BytesN::from_array(&env, &[7u8; 32]);
        client.launch_seal(&commitment);

        assert!(client.sealed());
        assert_eq!(
            client.launch_seal_info(),
            Some(LaunchSeal {
                commitment: commitment.clone(),
                ledger: 1234,
                supply: 1_000_000_0000000i128,
            })
        );

        let events = env.events().all();
        assert_eq!(
            events.slice(events.len() - 1..),
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("seal"),).into_val(&env),
                    (commitment, 1234u32, 1_000_000_0000000i128).into_val(&env)
                )
            ]
        );
    }

    #[test]
    fn test_launch_seal_supply_is_frozen_at_seal_time() {
        let (env, client, _, user) = setup();
        client.launch_seal(&BytesN::from_array(&env, &[1u8; 32]));
        client.mint(&user, &500i128);
        assert_eq!(client.launch_seal_info().unwrap().supply, 1_000_000_0000000i128);
    }

    #[test]
    #[should_panic(expected = "launch already sealed")]
    fn test_launch_seal_only_once() {
        let (env, client, _, _) = setup();
        client.launch_seal(&BytesN::from_array(&env, &[1u8; 32]));
        client.launch_seal(&BytesN::from_array(&env, &[2u8; 32]));
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_launch_seal_fails_after_revoke_admin() {
        let (env, client, _, _) = setup();
        client.revoke_admin();
        client.launch_seal(&BytesN::from_array(&env, &[1u8; 32]));
    }

    #[test]
    #[should_panic]
    fn test_launch_seal_requires_admin_auth() {
        let env = Env::default();
        let contract_id = env.register_contract(None, TokenContract);
        let client = TokenContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        env.mock_all_auths();
        client.initialize(
            &admin,
            &7u32,
            &String::from_str(&env, "TestToken"),
            &String::from_str(&env, "TST"),
            &0i128,
            &None,
            &false,
            &false,
            &None,
        );
        env.set_auths(&[]);
        client.launch_seal(&BytesN::from_array(&env, &[1u8; 32]));
    }
}
