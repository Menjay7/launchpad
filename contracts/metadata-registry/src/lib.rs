#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Bytes, BytesN, Env, String, Vec,
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
/// `extend_ttl` site clamps to it.
const TTL_LEDGERS: u32 = 365 * 24 * 60 * 60 / 5;

/// Largest page a caller may request from `get_records_paginated`.
///
/// Same bound as the factory's deployment index (#469): a caller-chosen
/// page size is clamped in-contract so a single read cannot burn the
/// invocation's compute budget on an arbitrarily large vector.
const MAX_PAGE: u32 = 100;

/// Longest metadata JSON, in bytes, that `register` will hash. The document
/// itself is not stored — only its keccak256 — but the host still has to
/// ingest the argument, so the length is bounded.
const MAX_METADATA_LEN: u32 = 4096;

/// Longest token name hashed into `name_digest`. Matches the deploy form's
/// 32-character cap with headroom for on-chain names that never went
/// through the UI.
const MAX_NAME_LEN: u32 = 256;

/// Longest symbol stored as an indexed field.
const MAX_SYMBOL_LEN: u32 = 32;

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// Address allowed to `upgrade`. Set once on `initialize`.
    Admin,
    /// Set once on the first successful `initialize` and never removed.
    Initialized,
    /// The factory allowed to call `register`. Written at `initialize`.
    Factory,
    /// Number of registered tokens (used to derive `TokenAt` slots).
    RecordCount,
    /// Enumerated token address, index `0..RecordCount`.
    TokenAt(u32),
    /// Indexed launch record keyed by token contract ID.
    Record(Address),
}

/// Compact, indexed launch record.
///
/// The off-chain JSON (description, logo, socials, …) lives wherever the
/// creator pins it — IPFS by default. This struct is what a third party
/// can enumerate and filter without fetching every document: `symbol` in
/// the clear, `name_digest` / `metadata_digest` as keccak256 commitments,
/// plus the launch facts (`creator`, `launch_ledger`, `initial_supply`).
#[derive(Clone)]
#[contracttype]
pub struct MetadataRecord {
    pub symbol: String,
    pub name_digest: BytesN<32>,
    pub creator: Address,
    pub launch_ledger: u32,
    pub initial_supply: i128,
    pub metadata_digest: Option<BytesN<32>>,
}

/// One page row: the token this record describes, plus the record.
#[derive(Clone)]
#[contracttype]
pub struct IndexedLaunch {
    pub token: Address,
    pub record: MetadataRecord,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    /// A getter or `register` ran before `initialize`.
    NotInitialized = 1,
    /// `initialize` was called on a contract that is already initialized.
    AlreadyInitialized = 2,
    /// `register` was called for a token that already has a record.
    AlreadyRegistered = 3,
    /// Name, symbol, or metadata JSON exceeded its length bound, or was empty
    /// when a value is required.
    InvalidMetadata = 4,
    /// WASM hash supplied to `upgrade` is the all-zeros sentinel.
    InvalidWasmHash = 5,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// Content-addressed token metadata registry.
///
/// Keyed by token contract ID. The factory writes the entry in the same
/// transaction as `deploy_token`, so a launch that reverts never leaves a
/// dangling record, and a successful launch is immediately enumerable by
/// indexers that do not depend on SoroPad's API.
#[contract]
pub struct MetadataRegistryContract;

#[contractimpl]
impl MetadataRegistryContract {
    /// Initialize the registry.
    ///
    /// `admin` may later `upgrade` the WASM. `factory` is the only address
    /// allowed to `register` — in production that is the launchpad factory,
    /// which authenticates as itself when it invokes this contract.
    pub fn initialize(env: Env, admin: Address, factory: Address) {
        if env.storage().instance().has(&DataKey::Initialized) {
            panic_with_error!(&env, RegistryError::AlreadyInitialized);
        }
        admin.require_auth();

        let storage = env.storage().instance();
        storage.set(&DataKey::Initialized, &true);
        storage.set(&DataKey::Admin, &admin);
        storage.set(&DataKey::Factory, &factory);
        let ttl = Self::_ttl_ledgers(&env);
        storage.extend_ttl(ttl, ttl);
        env.events()
            .publish((symbol_short!("init"),), (admin, factory));
    }

    /// Record a launch. Factory only; write-once per token.
    ///
    /// `metadata` is the canonical JSON document (or `None` if the creator
    /// supplied nothing to commit to). The registry stores `keccak256` of
    /// those bytes alongside the indexed fields — it does not store the
    /// JSON itself.
    pub fn register(
        env: Env,
        token: Address,
        symbol: String,
        name: String,
        creator: Address,
        initial_supply: i128,
        metadata: Option<String>,
    ) {
        Self::_require_factory(&env);

        if symbol.len() == 0 || symbol.len() > MAX_SYMBOL_LEN {
            panic_with_error!(&env, RegistryError::InvalidMetadata);
        }
        if name.len() == 0 || name.len() > MAX_NAME_LEN {
            panic_with_error!(&env, RegistryError::InvalidMetadata);
        }

        let metadata_digest = match metadata {
            Some(json) => {
                if json.len() == 0 || json.len() > MAX_METADATA_LEN {
                    panic_with_error!(&env, RegistryError::InvalidMetadata);
                }
                Some(Self::_keccak_string(&env, &json, MAX_METADATA_LEN))
            }
            None => None,
        };

        let key = DataKey::Record(token.clone());
        if env.storage().persistent().has(&key) {
            panic_with_error!(&env, RegistryError::AlreadyRegistered);
        }

        let record = MetadataRecord {
            symbol: symbol.clone(),
            name_digest: Self::_keccak_string(&env, &name, MAX_NAME_LEN),
            creator: creator.clone(),
            launch_ledger: env.ledger().sequence(),
            initial_supply,
            metadata_digest: metadata_digest.clone(),
        };

        env.storage().persistent().set(&key, &record);

        let count = Self::_record_count(&env);
        let at_key = DataKey::TokenAt(count);
        env.storage().persistent().set(&at_key, &token);
        env.storage()
            .persistent()
            .set(&DataKey::RecordCount, &(count + 1));

        let ttl_ledgers = Self::_ttl_ledgers(&env);
        env.storage()
            .persistent()
            .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
        env.storage()
            .persistent()
            .extend_ttl(&at_key, ttl_ledgers, ttl_ledgers);
        env.storage().persistent().extend_ttl(
            &DataKey::RecordCount,
            ttl_ledgers,
            ttl_ledgers,
        );

        env.events().publish(
            (symbol_short!("register"), token),
            (
                symbol,
                record.name_digest,
                creator,
                record.launch_ledger,
                initial_supply,
                metadata_digest,
            ),
        );
    }

    /// Return the indexed record for `token`, or `None` if it was never
    /// registered.
    pub fn get_record(env: Env, token: Address) -> Option<MetadataRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Record(token))
    }

    /// Return the number of tokens registered.
    pub fn get_record_count(env: Env) -> u32 {
        Self::_record_count(&env)
    }

    /// Return a page of indexed launches, newest not implied — insertion
    /// order, index `0` is the first token registered.
    ///
    /// `limit` is clamped to [`MAX_PAGE`].
    pub fn get_records_paginated(env: Env, start: u32, limit: u32) -> Vec<IndexedLaunch> {
        let total = Self::_record_count(&env);
        if start >= total {
            return Vec::new(&env);
        }

        let end = start.saturating_add(limit.min(MAX_PAGE)).min(total);
        let mut page = Vec::new(&env);
        let mut i = start;
        while i < end {
            if let Some(token) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::TokenAt(i))
            {
                if let Some(record) = env
                    .storage()
                    .persistent()
                    .get::<DataKey, MetadataRecord>(&DataKey::Record(token.clone()))
                {
                    page.push_back(IndexedLaunch { token, record });
                }
            }
            i += 1;
        }
        page
    }

    /// Return the factory address allowed to `register`.
    pub fn get_factory(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Factory)
            .unwrap_or_else(|| panic_with_error!(&env, RegistryError::NotInitialized))
    }

    /// Return the admin address.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RegistryError::NotInitialized))
    }

    /// Upgrade this contract's WASM in place. Admin only.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RegistryError::NotInitialized));
        admin.require_auth();
        if new_wasm_hash == BytesN::from_array(&env, &[0; 32]) {
            panic_with_error!(&env, RegistryError::InvalidWasmHash);
        }
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("upgrade"),), new_wasm_hash);
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn _require_factory(env: &Env) {
        let factory: Address = env
            .storage()
            .instance()
            .get(&DataKey::Factory)
            .unwrap_or_else(|| panic_with_error!(env, RegistryError::NotInitialized));
        factory.require_auth();
    }

    fn _record_count(env: &Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::RecordCount)
            .unwrap_or(0)
    }

    fn _ttl_ledgers(env: &Env) -> u32 {
        TTL_LEDGERS.min(env.storage().max_ttl())
    }

    fn _keccak_string(env: &Env, value: &String, max_len: u32) -> BytesN<32> {
        let len = value.len();
        if len > max_len {
            panic_with_error!(env, RegistryError::InvalidMetadata);
        }
        let mut buf = [0u8; MAX_METADATA_LEN as usize];
        let bytes = &mut buf[..len as usize];
        value.copy_into_slice(bytes);
        env.crypto()
            .keccak256(&Bytes::from_slice(env, bytes))
            .into()
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
        Env, IntoVal,
    };

    const EXPECTED_TOPICS: [&str; 3] = ["init", "register", "upgrade"];

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

    fn setup(
        env: &Env,
    ) -> (
        Address,
        MetadataRegistryContractClient<'static>,
        Address,
        Address,
    ) {
        let contract_id = env.register_contract(None, MetadataRegistryContract);
        let client = MetadataRegistryContractClient::new(env, &contract_id);
        let admin = Address::generate(env);
        let factory = Address::generate(env);
        client.initialize(&admin, &factory);
        (contract_id, client, admin, factory)
    }

    fn keccak(env: &Env, s: &str) -> BytesN<32> {
        env.crypto()
            .keccak256(&Bytes::from_slice(env, s.as_bytes()))
            .into()
    }

    fn register_as_factory(
        env: &Env,
        contract_id: &Address,
        factory: &Address,
        client: &MetadataRegistryContractClient<'static>,
        token: &Address,
        symbol: &str,
        name: &str,
        creator: &Address,
        initial_supply: i128,
        metadata: Option<&str>,
    ) {
        let symbol_s = String::from_str(env, symbol);
        let name_s = String::from_str(env, name);
        let meta = metadata.map(|m| String::from_str(env, m));
        env.mock_auths(&[MockAuth {
            address: factory,
            invoke: &MockAuthInvoke {
                contract: contract_id,
                fn_name: "register",
                args: (
                    token.clone(),
                    symbol_s.clone(),
                    name_s.clone(),
                    creator.clone(),
                    initial_supply,
                    meta.clone(),
                )
                    .into_val(env),
                sub_invokes: &[],
            },
        }]);
        client.register(token, &symbol_s, &name_s, creator, &initial_supply, &meta);
    }

    #[test]
    fn test_initialize_sets_admin_and_factory_and_is_callable_once() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client, admin, factory) = setup(&env);

        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_factory(), factory);
        assert!(client.try_initialize(&admin, &factory).is_err());
    }

    #[test]
    fn test_register_stores_indexed_fields_and_keccak_digest() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, client, _, factory) = setup(&env);

        let token = Address::generate(&env);
        let creator = Address::generate(&env);
        let json = r#"{"name":"Factory Token","symbol":"FTK","description":"hi"}"#;
        env.ledger().set_sequence_number(42);

        register_as_factory(
            &env,
            &contract_id,
            &factory,
            &client,
            &token,
            "FTK",
            "Factory Token",
            &creator,
            1_000_000,
            Some(json),
        );

        let record = client.get_record(&token).unwrap();
        assert_eq!(record.symbol, String::from_str(&env, "FTK"));
        assert_eq!(record.name_digest, keccak(&env, "Factory Token"));
        assert_eq!(record.creator, creator);
        assert_eq!(record.launch_ledger, 42);
        assert_eq!(record.initial_supply, 1_000_000);
        assert_eq!(record.metadata_digest, Some(keccak(&env, json)));
        assert_eq!(client.get_record_count(), 1);

        let events = env.events().all();
        let last = events.slice(events.len() - 1..);
        assert_eq!(
            last,
            soroban_sdk::vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("register"), token.clone()).into_val(&env),
                    (
                        String::from_str(&env, "FTK"),
                        keccak(&env, "Factory Token"),
                        creator,
                        42u32,
                        1_000_000i128,
                        Some(keccak(&env, json)),
                    )
                        .into_val(&env)
                )
            ]
        );
    }

    #[test]
    fn test_register_without_metadata_leaves_digest_none() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, client, _, factory) = setup(&env);
        let token = Address::generate(&env);
        let creator = Address::generate(&env);

        register_as_factory(
            &env,
            &contract_id,
            &factory,
            &client,
            &token,
            "FTK",
            "Factory Token",
            &creator,
            1,
            None,
        );

        let record = client.get_record(&token).unwrap();
        assert_eq!(record.metadata_digest, None);
    }

    #[test]
    fn test_register_rejects_non_factory() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, client, _, _) = setup(&env);
        let attacker = Address::generate(&env);
        let token = Address::generate(&env);
        let creator = Address::generate(&env);
        let symbol = String::from_str(&env, "FTK");
        let name = String::from_str(&env, "Factory Token");

        env.mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "register",
                args: (
                    token.clone(),
                    symbol.clone(),
                    name.clone(),
                    creator.clone(),
                    1i128,
                    None::<String>,
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(client
            .try_register(&token, &symbol, &name, &creator, &1i128, &None)
            .is_err());
        // `factory.require_auth()` traps unless the factory is in the
        // auth chain, so a direct caller cannot write a record.
        assert_eq!(client.get_record(&token), None);
        assert_eq!(client.get_record_count(), 0);
    }

    #[test]
    fn test_register_is_write_once() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, client, _, factory) = setup(&env);
        let token = Address::generate(&env);
        let creator = Address::generate(&env);

        register_as_factory(
            &env, &contract_id, &factory, &client, &token, "FTK", "A", &creator, 1, None,
        );
        env.mock_all_auths();
        assert_eq!(
            client.try_register(
                &token,
                &String::from_str(&env, "FTK"),
                &String::from_str(&env, "A"),
                &creator,
                &1i128,
                &None
            ),
            Err(Ok(RegistryError::AlreadyRegistered.into()))
        );
    }

    #[test]
    fn test_register_rejects_oversized_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client, _, _factory) = setup(&env);
        let token = Address::generate(&env);
        let creator = Address::generate(&env);

        let too_long = String::from_str(&env, &"x".repeat((MAX_METADATA_LEN as usize) + 1));
        assert_eq!(
            client.try_register(
                &token,
                &String::from_str(&env, "FTK"),
                &String::from_str(&env, "Name"),
                &creator,
                &1i128,
                &Some(too_long)
            ),
            Err(Ok(RegistryError::InvalidMetadata.into()))
        );
    }

    #[test]
    fn test_get_records_paginated_returns_pages() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, client, _, factory) = setup(&env);
        let creator = Address::generate(&env);

        let mut tokens = Vec::new(&env);
        for i in 0u8..3 {
            let token = Address::generate(&env);
            register_as_factory(
                &env,
                &contract_id,
                &factory,
                &client,
                &token,
                "FTK",
                "Name",
                &creator,
                i as i128,
                None,
            );
            tokens.push_back(token);
            env.mock_all_auths();
        }

        let page1 = client.get_records_paginated(&0u32, &2u32);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap().token, tokens.get(0).unwrap());
        assert_eq!(page1.get(1).unwrap().token, tokens.get(1).unwrap());

        let page2 = client.get_records_paginated(&2u32, &2u32);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2.get(0).unwrap().token, tokens.get(2).unwrap());
        assert_eq!(client.get_records_paginated(&3u32, &2u32).len(), 0);
    }

    #[test]
    fn test_get_record_unknown_token_is_none() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client, _, _) = setup(&env);
        assert_eq!(client.get_record(&Address::generate(&env)), None);
    }

    #[test]
    fn test_upgrade_rejects_zero_hash() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client, _, _) = setup(&env);
        assert_eq!(
            client.try_upgrade(&BytesN::from_array(&env, &[0; 32])),
            Err(Ok(RegistryError::InvalidWasmHash.into()))
        );
    }
}
