/**
 * Generated bindings barrel reexport.
 *
 * This module exposes the generated clients for all contracts that
 * publish events and are part of the indexer-facing contract surface:
 * token, vesting, airdrop, and factory.
 *
 * Keep this in sync with `docs/events.json` and the contracts under
 * `contracts/`. The CI guard fails when a contract has no entry in
 * `docs/events.json`.
 */

export * as token from "./token";
export * as vesting from "./vesting";
export * as airdrop from "./airdrop";
export * as factory from "./factory";
