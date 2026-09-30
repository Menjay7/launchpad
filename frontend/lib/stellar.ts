import * as StellarSdk from "@stellar/stellar-sdk";
import { type NetworkConfig } from "../types/network";
import { fetchIndexedEvents } from "./indexer";
import { wrapRpcCall } from "./soroban";
import { buildRevokeAllowanceArgs } from "./transactionSimulator";

// ---------------------------------------------------------------------------
// Config — defaults to Stellar Testnet, overridable via localStorage
// ---------------------------------------------------------------------------
const DEFAULT_HORIZON_URL =
  process.env.NEXT_PUBLIC_HORIZON_URL ?? "https://horizon-testnet.stellar.org";

function getHorizonUrl(): string {
  if (typeof window !== "undefined") {
    return localStorage.getItem("soropad_horizon_url") || DEFAULT_HORIZON_URL;
  }
  return DEFAULT_HORIZON_URL;
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------
export interface VerificationResult {
  status: "verified" | "modified" | "unknown";
  deployedHash: string | null;
  referenceHash: string | null;
  referenceVersion: string | null;
  isLocked: boolean;
}

export interface WasmManifestEntry {
  wasm_hash: string;
  source_tag: string;
  build_ledger: string;
}

export interface WasmManifest {
  token: {
    latest: string;
    versions: Record<string, WasmManifestEntry>;
    build_info: { sdk_version: string; rust_version: string; profile: string };
  };
  vesting: {
    latest: string;
    versions: Record<string, WasmManifestEntry>;
    build_info: { sdk_version: string; rust_version: string; profile: string };
  };
  factory?: {
    deployments: Record<string, FactoryManifestEntry | undefined>;
  };
}

export interface FactoryManifestEntry {
  address: string;
  wasm_hash: string;
}

export async function fetchWasmManifest(): Promise<WasmManifest | null> {
  try {
    const res = await fetch("/api/wasm-manifest");
    if (!res.ok) return null;
    return (await res.json()) as WasmManifest;
  } catch {
    return null;
  }
}

export interface TokenInfo {
  name: string;
  symbol: string;
  decimals: number;
  totalSupply: string;
  circulatingSupply: string;
  admin: string;
  contractId: string;
  maxSupply?: string | null;
  contractUri?: string;
  complianceNode?: string | null;
  authorizationRequired?: boolean;
  authorizationRevocable?: boolean;
  isPaused?: boolean;
  isLocked?: boolean;
}

/** Per-wallet authorization/freeze state for a token contract. */
export interface WalletTokenState {
  isAuthorized: boolean;
  isFrozen: boolean;
}

export interface TokenHolder {
  address: string;
  balance: string;
  sharePercent: number;
}

export interface VestingScheduleInfo {
  recipient: string;
  totalAmount: string;
  cliffLedger: number;
  endLedger: number;
  released: string;
  revoked: boolean;
  scheduleIndex?: number;
  scheduleCount?: number;
}

export interface TransactionItem {
  type: "mint" | "burn" | "clawback" | "transfer";
  from?: string;
  to?: string;
  amount: string;
  timestamp: number;
  ledger: number;
  id: string;
}

/**
 * Metadata describing how much of a contract's history a page actually covers.
 *
 * When Mercury is unavailable the RPC fallback can only read back to the
 * oldest ledger still retained by the node (roughly 1000 ledgers, ~83
 * minutes). Callers must surface this so the UI does not present a truncated
 * window as the token's complete history.
 */
export interface HistoryWindow {
  /** Oldest ledger included in this page, or null when unknown. */
  startLedger: number | null;
  /** True when the page does not reach back to the contract's first ledger. */
  truncated: boolean;
}

export interface TokenAllowanceInfo {
  spenderAddress: string;
  amount: string;
  expirationLedger: number;
  isExpired: boolean;
}

// ---------------------------------------------------------------------------
// Soroban RPC helpers
// ---------------------------------------------------------------------------

async function simulateAndAssembleTransaction(
  tx: StellarSdk.Transaction,
  config: NetworkConfig,
) {
  const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
  const simulated = await rpc.simulateTransaction(tx);

  if (StellarSdk.rpc.Api.isSimulationError(simulated)) {
    throw new Error(`Simulation failed: ${simulated.error}`);
  }

  if (!StellarSdk.rpc.Api.isSimulationSuccess(simulated)) {
    throw new Error("Transaction simulation was not successful");
  }

  return StellarSdk.rpc.assembleTransaction(tx, simulated);
}

/**
 * Simulate a read-only Soroban contract invocation and return the result xdr.
 */
export async function simulateCall(
  contractId: string,
  method: string,
  config: NetworkConfig,
  args: StellarSdk.xdr.ScVal[] = [],
): Promise<StellarSdk.xdr.ScVal> {
  return wrapRpcCall(
    async () => {
      const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
      const contract = new StellarSdk.Contract(contractId);
      const account = new StellarSdk.Account(
        "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
        "0",
      );

      const tx = new StellarSdk.TransactionBuilder(account, {
        fee: "100",
        networkPassphrase: config.passphrase,
      })
        .addOperation(contract.call(method, ...args))
        .setTimeout(30)
        .build();

      const sim = await rpc.simulateTransaction(tx);

      if (StellarSdk.rpc.Api.isSimulationError(sim)) {
        throw new Error(`Soroban simulation error (${method}): ${sim.error}`);
      }

      if (!StellarSdk.rpc.Api.isSimulationSuccess(sim) || !sim.result) {
        throw new Error(`Soroban simulation failed for ${method}`);
      }

      return sim.result.retval;
    },
    { operation: `simulate ${method}`, silent: true },
  );
}

/**
 * Read the current WASM hash of a deployed Soroban contract.
 *
 * Uses the RPC getLedgerEntries endpoint to fetch the contract instance
 * entry, which contains the executable WASM hash. Returns the hash as a
 * lowercase hex string, or null when the entry cannot be read.
 */
export async function getContractWasmHash(
  contractId: string,
  config: NetworkConfig,
): Promise<string | null> {
  try {
    const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
    const instanceKey = StellarSdk.xdr.ScVal.scvLedgerKeyContractInstance();
    const entry = await rpc.getContractData(contractId, instanceKey);
    const contractData = (entry as { val: StellarSdk.xdr.LedgerEntryData }).val.contractData();
    const scVal = contractData.val();
    if (scVal.switch() !== StellarSdk.xdr.ScValType.scvContractInstance()) {
      return null;
    }
    const instance = scVal.instance();
    const executable = instance.executable();
    if (executable.switch() !== StellarSdk.xdr.ContractExecutableType.contractExecutableWasm()) {
      return null;
    }
    const wasmHashBuf = executable.wasmHash() as Buffer;
    return Buffer.from(wasmHashBuf).toString("hex");
  } catch {
    return null;
  }
}

export type FactoryVerification =
  | { ok: true; verified: boolean }
  | { ok: false; reason: string; expected?: string; actual?: string };

/**
 * Check the configured factory against the audited manifest for the active
 * network: the address must be the one the manifest names and the on-chain
 * WASM hash must equal the manifest hash. Fields the manifest leaves blank
 * are not checked (`verified: false` when nothing could be compared).
 */
export async function verifyFactory(
  factoryAddress: string,
  config: NetworkConfig,
): Promise<FactoryVerification> {
  const manifest = await fetchWasmManifest();
  const entry = manifest?.factory?.deployments?.[config.network];
  if (!entry || (!entry.address && !entry.wasm_hash)) {
    return { ok: true, verified: false };
  }

  if (entry.address && entry.address !== factoryAddress) {
    return {
      ok: false,
      reason: "The configured factory address is not the audited factory for this network.",
      expected: entry.address,
      actual: factoryAddress,
    };
  }

  if (entry.wasm_hash) {
    const onChain = await getContractWasmHash(factoryAddress, config);
    if (!onChain) {
      return {
        ok: false,
        reason: "Could not read the factory's on-chain WASM hash.",
        expected: entry.wasm_hash,
      };
    }
    if (onChain.toLowerCase() !== entry.wasm_hash.toLowerCase()) {
      return {
        ok: false,
        reason: "The factory's on-chain WASM hash does not match the audited build.",
        expected: entry.wasm_hash,
        actual: onChain,
      };
    }
  }

  return { ok: true, verified: true };
}

function encodeTopicSymbol(symbol: string): string {
  return StellarSdk.nativeToScVal(symbol, { type: "symbol" }).toXDR("base64");
}

export function toScVal(value: unknown): StellarSdk.xdr.ScVal | null {
  if (!value) return null;
  if (typeof value === "string") {
    try {
      return StellarSdk.xdr.ScVal.fromXDR(value, "base64");
    } catch {
      return null;
    }
  }

  if (
    typeof value === "object" &&
    value !== null &&
    typeof (value as StellarSdk.xdr.ScVal).switch === "function"
  ) {
    return value as StellarSdk.xdr.ScVal;
  }

  return null;
}

export function readEventTopics(event: unknown): unknown[] {
  const e = event as { topic?: unknown; topics?: unknown };
  if (Array.isArray(e.topic)) return e.topic;
  if (Array.isArray(e.topics)) return e.topics;
  return [];
}

export function readEventLedger(event: unknown): number {
  const e = event as {
    ledger?: unknown;
    ledger_seq?: unknown;
    ledger_sequence?: unknown;
    ledgerSequence?: unknown;
  };
  const val =
    e.ledger ?? e.ledger_seq ?? e.ledger_sequence ?? e.ledgerSequence ?? 0;
  return typeof val === "number" ? val : Number(val) || 0;
}

export function readEventId(event: unknown, fallback: string): string {
  const e = event as { id?: unknown; event_id?: unknown; eventId?: unknown };
  const val = e.id ?? e.event_id ?? e.eventId;
  return typeof val === "string" ? val : fallback;
}

export function readEventTxHash(event: unknown): string {
  const e = event as { tx_hash?: unknown; txHash?: unknown; hash?: unknown };
  const val = e.tx_hash ?? e.txHash ?? e.hash;
  return typeof val === "string" ? val : "";
}

export function readEventTimestamp(event: unknown): string {
  const e = event as {
    timestamp?: unknown;
    ledger_timestamp?: unknown;
    ledgerTimestamp?: unknown;
    created_at?: unknown;
    createdAt?: unknown;
  };
  const val =
    e.timestamp ??
    e.ledger_timestamp ??
    e.ledgerTimestamp ??
    e.created_at ??
    e.createdAt;

  if (typeof val === "string") return val;
  if (typeof val === "number") return new Date(val * 1000).toISOString();
  return new Date(0).toISOString();
}

export function readEventTimestampNumber(event: unknown): number {
  const iso = readEventTimestamp(event);
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? 0 : Math.floor(ms / 1000);
}

export async function fetchTokenAllowance(
  contractId: string,
  ownerAddress: string,
  spenderAddress: string,
  config: NetworkConfig,
): Promise<bigint> {
  const args = [
    new StellarSdk.Address(ownerAddress).toScVal(),
    new StellarSdk.Address(spenderAddress).toScVal(),
  ];
  const allowanceVal = await simulateCall(
    contractId,
    "allowance",
    config,
    args,
  );
  return BigInt(decodeI128(allowanceVal));
}

export interface AllowanceWithExpiration {
  amount: bigint;
  expirationLedger: number;
}

export async function fetchAllowanceWithExpiration(
  contractId: string,
  ownerAddress: string,
  spenderAddress: string,
  config: NetworkConfig,
): Promise<AllowanceWithExpiration> {
  const args = [
    new StellarSdk.Address(ownerAddress).toScVal(),
    new StellarSdk.Address(spenderAddress).toScVal(),
  ];
  const allowanceVal = await simulateCall(
    contractId,
    "allowance",
    config,
    args,
  );

  const amount = BigInt(decodeI128(allowanceVal));

  let expirationLedger = 0;
  try {
    if (
      allowanceVal.switch() ===
      StellarSdk.xdr.ScValType.scvVec()
    ) {
      const items = allowanceVal.vec();
      if (items && items.length >= 2) {
        expirationLedger = decodeU32(items[1]);
      }
    }
  } catch {
    // If parsing the tuple fails, default to 0
  }

  return { amount, expirationLedger };
}

export async function fetchApprovedSpendersFromEvents(params: {
  contractId: string;
  ownerAddress: string;
  config: NetworkConfig;
  maxPages?: number;
}): Promise<string[]> {
  const { contractId, ownerAddress, config, maxPages = 5 } = params;
  const spenders = new Set<string>();

  const topicApprove = encodeTopicSymbol("approve");
  const pageSize = 200;

  let cursor: string | undefined;
  for (let page = 0; page < maxPages; page++) {
    const { events, nextCursor } = await fetchIndexedEvents(
      contractId,
      config,
      {
        topics: [topicApprove],
        limit: pageSize,
        cursor,
      },
    );

    if (events.length === 0) break;

    for (const event of events) {
      try {
        if (event.topic.length < 3) continue;

        const topic0 = toScVal(event.topic[0]);
        const topic1 = toScVal(event.topic[1]);
        const topic2 = toScVal(event.topic[2]);
        if (!topic0 || !topic1 || !topic2) continue;

        const symbol = decodeString(topic0);
        if (symbol !== "approve") continue;

        const from = decodeAddress(topic1);
        const spender = decodeAddress(topic2);

        if (from === ownerAddress) {
          spenders.add(spender);
        }
      } catch {
        // ignore malformed events
      }
    }

    if (!nextCursor) break;
    cursor = nextCursor;
  }

  return Array.from(spenders);
}

/** Decode an ScVal string (symbol or string type). */
export function decodeString(val: StellarSdk.xdr.ScVal): string {
  switch (val.switch()) {
    case StellarSdk.xdr.ScValType.scvSymbol():
      return val.sym().toString();
    case StellarSdk.xdr.ScValType.scvString():
      return val.str().toString();
    default:
      return val.value()?.toString() ?? "";
  }
}

/** Decode an ScVal 128-bit integer to a bigint string. */
export function decodeI128(val: StellarSdk.xdr.ScVal): string {
  const parts = val.i128();
  const hi = BigInt(parts.hi().toString());
  const lo = BigInt(parts.lo().toString());
  return ((hi << BigInt(64)) + lo).toString();
}

/** Decode an ScVal u32. */
export function decodeU32(val: StellarSdk.xdr.ScVal): number {
  return val.u32();
}

/** Decode an ScVal address to a string. */
export function decodeAddress(val: StellarSdk.xdr.ScVal): string {
  return StellarSdk.Address.fromScVal(val).toString();
}

function toU32ScVal(value: number): StellarSdk.xdr.ScVal {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff_ffff) {
    throw new Error("Invalid u32 value");
  }
  return StellarSdk.nativeToScVal(BigInt(value), { type: "u32" });
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Fetch full token metadata from a Soroban SEP-41 token contract.
 */
/**
 * Validate if a contract implements the SEP-41 token standard.
 * This function attempts to call the required SEP-41 methods to verify
 * that the contract is a valid token contract.
 */
/**
 * Result of probing a contract for a SEP-41 token interface.
 *
 * Three states, not two: a well-formed contract ID can point at an account
 * (not a contract at all), at a contract that simply does not export the
 * token interface, or at a real token. Collapsing the middle state into
 * either of the others is what made every well-formed `C...` address render
 * as an empty token dashboard.
 */
export type TokenContractProbe =
  | { status: "token" }
  | { status: "not-a-contract"; error: string }
  | { status: "no-token-interface"; error: string }
  | { status: "unreachable"; error: string };

/**
 * Distinguish "this contract has no `name()` export" from "the RPC call
 * itself failed".
 *
 * `simulateTransaction` reports a missing export as a simulation error whose
 * text names the missing function, while a transport failure rejects the
 * promise outright. That difference is the whole probe: it is the same
 * pattern `set_compliance_node` uses on the contract side to decide whether
 * an optional getter exists.
 */
function isMissingExportError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return (
    /missing\s+(value|function|method|export)/i.test(message) ||
    /function\s+['"`]?[a-z_][a-z0-9_]*['"`]?\s+(not\s+found|does\s+not\s+exist)/i.test(
      message,
    ) ||
    /has\s+no\s+member/i.test(message) ||
    /not\s+found\s+in\s+contract/i.test(message)
  );
}

/**
 * Probe a contract once and report which of the three states it is in.
 *
 * Runs a single `name()` simulation: a success means the contract exports the
 * token interface, a missing-export simulation error means it is a contract
 * without one, and anything else (transport, bad ID) is reported as
 * unreachable so the caller can keep showing the transport-failure copy.
 */
export async function probeTokenContract(
  contractId: string,
  config: NetworkConfig,
): Promise<TokenContractProbe> {
  try {
    const nameVal = await simulateCall(contractId, "name", config);
    try {
      decodeString(nameVal);
    } catch {
      return {
        status: "no-token-interface",
        error: "Contract does not implement a decodable 'name()' method",
      };
    }
    return { status: "token" };
  } catch (error) {
    const message =
      error instanceof Error ? error.message : "Unknown validation error";

    if (isMissingExportError(error)) {
      return { status: "no-token-interface", error: message };
    }

    // A contract ID that does not resolve to a deployed contract surfaces as
    // a ledger-entry lookup failure rather than a missing export.
    if (/contract\s+(not\s+found|does\s+not\s+exist)/i.test(message)) {
      return { status: "not-a-contract", error: message };
    }

    return { status: "unreachable", error: message };
  }
}

export async function validateTokenContract(
  contractId: string,
  config: NetworkConfig,
): Promise<{ isValid: boolean; error?: string }> {
  try {
    // Try to call the required SEP-41 methods
    const [nameResult, symbolResult, decimalsResult] = await Promise.allSettled(
      [
        simulateCall(contractId, "name", config),
        simulateCall(contractId, "symbol", config),
        simulateCall(contractId, "decimals", config),
      ],
    );

    // Check if all required methods are available
    if (nameResult.status === "rejected") {
      return {
        isValid: false,
        error: "Contract does not implement 'name()' method",
      };
    }

    if (symbolResult.status === "rejected") {
      return {
        isValid: false,
        error: "Contract does not implement 'symbol()' method",
      };
    }

    if (decimalsResult.status === "rejected") {
      return {
        isValid: false,
        error: "Contract does not implement 'decimals()' method",
      };
    }

    // Try to decode the values to ensure they return the correct types
    try {
      decodeString(nameResult.value);
      decodeString(symbolResult.value);
      decodeU32(decimalsResult.value);
    } catch {
      return {
        isValid: false,
        error: "Contract methods return invalid data types",
      };
    }

    // Optionally check for other common SEP-41 methods
    const [balanceResult, totalSupplyResult] = await Promise.allSettled([
      simulateCall(contractId, "balance", config, [
        StellarSdk.Address.fromString(
          "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
        ).toScVal(),
      ]),
      simulateCall(contractId, "total_supply", config),
    ]);

    if (
      balanceResult.status === "rejected" &&
      totalSupplyResult.status === "rejected"
    ) {
      return {
        isValid: false,
        error: "Contract does not implement required balance or supply methods",
      };
    }

    return { isValid: true };
  } catch (error) {
    return {
      isValid: false,
      error:
        error instanceof Error ? error.message : "Unknown validation error",
    };
  }
}

export async function fetchTokenInfo(
  contractId: string,
  config: NetworkConfig,
): Promise<TokenInfo> {
  return wrapRpcCall(() => _fetchTokenInfo(contractId, config), {
    operation: "Fetch token info",
  });
}

async function _fetchTokenInfo(
  contractId: string,
  config: NetworkConfig,
): Promise<TokenInfo> {
  // First validate that this is a SEP-41 token contract
  const validation = await validateTokenContract(contractId, config);
  if (!validation.isValid) {
    throw new Error(`Invalid token contract: ${validation.error}`);
  }

  const [nameVal, symbolVal, decimalsVal, adminVal] = await Promise.all([
    simulateCall(contractId, "name", config),
    simulateCall(contractId, "symbol", config),
    simulateCall(contractId, "decimals", config),
    simulateCall(contractId, "admin_if_any", config),
  ]);

  const decimals = decodeU32(decimalsVal);

  let totalSupply = "N/A";
  let circulatingSupply = "N/A";
  try {
    const supplyVal = await simulateCall(contractId, "total_supply", config);
    const rawSupply = decodeI128(supplyVal);
    totalSupply = formatTokenAmount(rawSupply, decimals);
    circulatingSupply = totalSupply;
  } catch {
    // total_supply not implemented on this contract
  }

  let maxSupply: string | null = null;
  try {
    const maxVal = await simulateCall(contractId, "max_supply", config);
    if (maxVal && maxVal.switch() !== StellarSdk.xdr.ScValType.scvVoid()) {
      const rawMax = decodeI128(maxVal);
      maxSupply = formatTokenAmount(rawMax, decimals);
    }
  } catch {
    // max_supply not implemented or not accessible
  }

  let contractUri: string | undefined;
  try {
    const uriVal = await simulateCall(contractId, "contract_uri", config);
    if (uriVal && uriVal.switch() !== StellarSdk.xdr.ScValType.scvVoid()) {
      contractUri = decodeString(uriVal);
    }
  } catch {
    // contract_uri may not be implemented or accessible; ignore.
  }

  let complianceNode: string | null = null;
  try {
    const nodeVal = await simulateCall(contractId, "compliance_node", config);
    if (nodeVal && nodeVal.switch() !== StellarSdk.xdr.ScValType.scvVoid()) {
      complianceNode = decodeAddress(nodeVal);
    }
  } catch {
    // compliance_node may not be implemented or accessible; ignore.
  }

  let authorizationRequired = false;
  let authorizationRevocable = false;
  try {
    const reqVal = await simulateCall(contractId, "authorization_required", config);
    if (reqVal) authorizationRequired = Boolean(reqVal.b());
    const revVal = await simulateCall(contractId, "authorization_revocable", config);
    if (revVal) authorizationRevocable = Boolean(revVal.b());
  } catch {
    // authorization checks not implemented or accessible; ignore.
  }

  let isPaused = false;
  try {
    const pausedVal = await simulateCall(contractId, "is_paused", config);
    isPaused = Boolean(pausedVal.b());
  } catch {
    // is_paused not implemented on this contract; assume not paused.
  }

  let isLocked = false;
  try {
    const lockedVal = await simulateCall(contractId, "is_locked", config);
    isLocked = Boolean(lockedVal.b());
  } catch {
    // is_locked not implemented on this contract; assume not locked.
  }

  return {
    name: decodeString(nameVal),
    symbol: decodeString(symbolVal),
    decimals,
    totalSupply,
    circulatingSupply,
    admin:
      adminVal &&
      typeof (adminVal as StellarSdk.xdr.ScVal).switch === "function" &&
      (adminVal as StellarSdk.xdr.ScVal).switch() !==
        StellarSdk.xdr.ScValType.scvVoid()
        ? decodeAddress(adminVal as StellarSdk.xdr.ScVal)
        : "N/A",
    contractId,
    maxSupply,
    contractUri,
    complianceNode,
    authorizationRequired,
    authorizationRevocable,
    isPaused,
    isLocked,
  };
}

/**
 * Fetch a specific wallet's authorization/freeze state on a token contract.
 * Guarded so contracts missing `is_authorized`/`is_frozen` degrade to the
 * permissive defaults (authorized, not frozen).
 */
export async function fetchWalletTokenState(
  contractId: string,
  address: string,
  config: NetworkConfig,
): Promise<WalletTokenState> {
  const addressScVal = new StellarSdk.Address(address).toScVal();

  let isAuthorized = true;
  try {
    const authVal = await simulateCall(
      contractId,
      "is_authorized",
      config,
      [addressScVal],
    );
    isAuthorized = Boolean(authVal.b());
  } catch {
    // is_authorized not implemented; assume authorized.
  }

  let isFrozen = false;
  try {
    const frozenVal = await simulateCall(contractId, "is_frozen", config, [
      addressScVal,
    ]);
    isFrozen = Boolean(frozenVal.b());
  } catch {
    // is_frozen not implemented; assume not frozen.
  }

  return { isAuthorized, isFrozen };
}

/**
 * Fetch a specific account's balance from a Soroban SEP-41 token contract.
 */
export async function fetchTokenBalance(
  contractId: string,
  accountId: string,
  config: NetworkConfig,
): Promise<string> {
  try {
    const args = [StellarSdk.nativeToScVal(accountId, { type: "address" })];
    const res = await simulateCall(contractId, "balance", config, args);

    if (!res) return "0.00";

    const rawBalance = decodeI128(res);

    const decimalsRes = await simulateCall(contractId, "decimals", config);
    const decimals = decimalsRes ? decodeU32(decimalsRes) : 7;

    return formatTokenAmount(rawBalance, decimals);
  } catch (err) {
    console.error(`Failed to fetch and decode balance for ${accountId}:`, err);
    return "0.00";
  }
}

/**
 * Fetch the top token holders.
 */
export async function fetchTopHolders(
  contractId: string,
  config: NetworkConfig,
  symbolHint?: string,
  issuerHint?: string,
  limit = 10,
): Promise<TokenHolder[]> {
  try {
    // Soroban-native token contracts do not have a Horizon asset issuer.
    // Returning an empty list here avoids accidentally mixing in holders
    // from an unrelated wrapped asset that happens to share the same symbol.
    if (contractId.startsWith("C")) {
      return [];
    }

    const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);
    const symbol =
      symbolHint ??
      decodeString(await simulateCall(contractId, "symbol", config));
    const decimals = decodeU32(
      await simulateCall(contractId, "decimals", config),
    );

    let issuer = issuerHint;
    if (!issuer) {
      // Resolve issuer for this asset code from Horizon stats.
      // If multiple issuers exist for same code, prefer one with largest supply.
      const assetsPage = await horizon
        .assets()
        .forCode(symbol)
        .limit(200)
        .call();
      if (assetsPage.records.length === 0) {
        return [];
      }
      const getAssetAmount = (record: unknown): string => {
        const raw = (record as { amount?: unknown }).amount;
        return typeof raw === "string" ? raw : "0";
      };
      issuer = assetsPage.records
        .slice()
        .sort(
          (a, b) =>
            parseFloat(getAssetAmount(b)) - parseFloat(getAssetAmount(a)),
        )[0].asset_issuer;
    }

    const asset = new StellarSdk.Asset(symbol, issuer);
    const [holdersPage, assetStats] = await Promise.all([
      horizon.accounts().forAsset(asset).limit(limit).order("desc").call(),
      horizon.assets().forCode(symbol).forIssuer(issuer).limit(1).call(),
    ]);

    const toRawAmount = (amount: string, tokenDecimals: number): bigint => {
      const [wholePart, fracPart = ""] = amount.split(".");
      const normalizedFrac = fracPart
        .padEnd(tokenDecimals, "0")
        .slice(0, tokenDecimals);
      return BigInt(`${wholePart}${normalizedFrac}`);
    };

    const totalSupplyRaw =
      assetStats.records.length > 0
        ? toRawAmount(
            (assetStats.records[0] as { amount?: string }).amount ?? "0",
            decimals,
          )
        : BigInt(0);

    const parsed = holdersPage.records
      .map((acc) => {
        const bal = acc.balances.find(
          (b) =>
            "asset_code" in b &&
            b.asset_code === symbol &&
            "asset_issuer" in b &&
            b.asset_issuer === issuer,
        );
        const rawBalance = toRawAmount(bal ? bal.balance : "0", decimals);
        return { address: acc.account_id, rawBalance };
      })
      .filter((row) => row.rawBalance > BigInt(0))
      .sort((a, b) =>
        a.rawBalance > b.rawBalance ? -1 : a.rawBalance < b.rawBalance ? 1 : 0,
      );

    return parsed.map(({ address, rawBalance }) => ({
      address,
      balance: formatTokenAmount(rawBalance.toString(), decimals),
      sharePercent:
        totalSupplyRaw > BigInt(0)
          ? Number((rawBalance * BigInt(10000)) / totalSupplyRaw) / 100
          : 0,
    }));
  } catch {
    return [];
  }
}

// ---------------------------------------------------------------------------
// Vesting helpers
// ---------------------------------------------------------------------------

function getStructField(
  entries: StellarSdk.xdr.ScMapEntry[],
  name: string,
): StellarSdk.xdr.ScVal {
  const entry = entries.find((e) => decodeString(e.key()) === name);
  if (!entry) throw new Error(`Missing struct field: ${name}`);
  return entry.val();
}

/**
 * Fetch the current ledger sequence number.
 */
export async function fetchCurrentLedger(
  config: NetworkConfig,
): Promise<number> {
  return wrapRpcCall(
    async () => {
      const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
      const result = await rpc.getLatestLedger();
      return result.sequence;
    },
    { operation: "Fetch latest ledger" },
  );
}

export async function fetchVestingScheduleCount(
  vestingContractId: string,
  recipient: string,
  config: NetworkConfig,
): Promise<number> {
  const recipientScVal = new StellarSdk.Address(recipient).toScVal();
  const result = await simulateCall(
    vestingContractId,
    "get_schedule_count",
    config,
    [recipientScVal],
  );
  return decodeU32(result);
}

async function resolveVestingScheduleIndex(params: {
  vestingContractId: string;
  recipient: string;
  config: NetworkConfig;
  scheduleIndex?: number;
}): Promise<{ scheduleIndex: number; scheduleCount: number }> {
  const { vestingContractId, recipient, config, scheduleIndex } = params;
  if (scheduleIndex !== undefined) {
    const scheduleCount = await fetchVestingScheduleCount(
      vestingContractId,
      recipient,
      config,
    );
    if (scheduleCount <= 0) {
      throw new Error("no schedule found");
    }
    if (scheduleIndex < 0 || scheduleIndex >= scheduleCount) {
      throw new Error("schedule index out of bounds");
    }
    return { scheduleIndex, scheduleCount };
  }

  const scheduleCount = await fetchVestingScheduleCount(
    vestingContractId,
    recipient,
    config,
  );
  if (scheduleCount <= 0) {
    throw new Error("no schedule found");
  }
  return { scheduleIndex: scheduleCount - 1, scheduleCount };
}

/** Entries per `get_schedules_paginated` call. Matches the dashboard page size. */
const SCHEDULE_PAGE_SIZE = 20;

/**
 * Fetch all vesting schedules for a recipient.
 * Replaces the N+1 pattern (get_schedule_count + N × get_schedule).
 *
 * Pages through `get_schedules_paginated` rather than calling
 * `get_all_schedules` (#466). The contract caps a recipient at
 * `MAX_SCHEDULES_PER_RECIPIENT` schedules, so the aggregate getter is now
 * bounded — but one response carrying the whole set still costs a simulation
 * proportional to their total grants, and the dashboard renders a screenful at
 * a time. Paging keeps each response small and per-call work constant as
 * grants accumulate.
 *
 * Pages are followed until one comes back shorter than the page size, which is
 * the only reliable "no more" signal: a short page can still be followed by
 * more entries.
 */
export async function fetchAllVestingSchedules(
  vestingContractId: string,
  recipient: string,
  config: NetworkConfig,
): Promise<VestingScheduleInfo[]> {
  const recipientScVal = new StellarSdk.Address(recipient).toScVal();

  // Callers use `scheduleCount` for "x of y", so it has to be the recipient's
  // real total rather than the size of whichever page happened to land last.
  const scheduleCount = await fetchVestingScheduleCount(
    vestingContractId,
    recipient,
    config,
  );
  if (scheduleCount <= 0) {
    return [];
  }

  const schedules: VestingScheduleInfo[] = [];
  for (let start = 0; start < scheduleCount; start += SCHEDULE_PAGE_SIZE) {
    const result = await simulateCall(
      vestingContractId,
      "get_schedules_paginated",
      config,
      [
        recipientScVal,
        nativeToScVal(BigInt(start), { type: "u32" }),
        nativeToScVal(BigInt(SCHEDULE_PAGE_SIZE), { type: "u32" }),
      ],
    );

    const vec = result.vec();
    if (!vec || vec.length === 0) {
      // The count said there was more; a short read here means the index moved
      // under us. Stop rather than spin.
      break;
    }

    for (let i = 0; i < vec.length; i++) {
      const fields = vec[i].map()!;
      schedules.push({
        recipient: decodeAddress(getStructField(fields, "recipient")),
        totalAmount: decodeI128(getStructField(fields, "total_amount")),
        cliffLedger: decodeU32(getStructField(fields, "cliff_ledger")),
        endLedger: decodeU32(getStructField(fields, "end_ledger")),
        released: decodeI128(getStructField(fields, "released")),
        revoked: getStructField(fields, "revoked").b(),
        // Global index, not page-relative: this value is passed back to
        // `release`, which addresses schedules absolutely.
        scheduleIndex: start + i,
        scheduleCount,
      });
    }
  }
  return schedules;
}

/**
 * Fetch a vesting schedule.
 */
export async function fetchVestingSchedule(
  vestingContractId: string,
  recipient: string,
  config: NetworkConfig,
  scheduleIndex?: number,
): Promise<VestingScheduleInfo> {
  const resolved = await resolveVestingScheduleIndex({
    vestingContractId,
    recipient,
    config,
    scheduleIndex,
  });
  const recipientScVal = new StellarSdk.Address(recipient).toScVal();
  const result = await simulateCall(vestingContractId, "get_schedule", config, [
    recipientScVal,
    toU32ScVal(resolved.scheduleIndex),
  ]);

  const fields = result.map()!;
  return {
    recipient: decodeAddress(getStructField(fields, "recipient")),
    totalAmount: decodeI128(getStructField(fields, "total_amount")),
    cliffLedger: decodeU32(getStructField(fields, "cliff_ledger")),
    endLedger: decodeU32(getStructField(fields, "end_ledger")),
    released: decodeI128(getStructField(fields, "released")),
    revoked: getStructField(fields, "revoked").b(),
    scheduleIndex: resolved.scheduleIndex,
    scheduleCount: resolved.scheduleCount,
  };
}

/**
 * Fetch the vested amount for a recipient's schedule directly from the
 * contract's `vested_amount` getter.
 *
 * This is the authoritative value — prefer it over any client-side
 * replication of the vesting formula. The on-chain formula is the source
 * of truth and may change (see issues #356, #357).
 */
export async function fetchVestedAmount(
  vestingContractId: string,
  recipient: string,
  config: NetworkConfig,
  scheduleIndex?: number,
): Promise<string> {
  const resolved = await resolveVestingScheduleIndex({
    vestingContractId,
    recipient,
    config,
    scheduleIndex,
  });
  const recipientScVal = new StellarSdk.Address(recipient).toScVal();
  const result = await simulateCall(
    vestingContractId,
    "vested_amount",
    config,
    [recipientScVal, toU32ScVal(resolved.scheduleIndex)],
  );
  return decodeI128(result);
}

/**
 * Fetch transaction history (events) for a token contract via the Mercury indexer.
 * Uses cursor-based pagination to walk past the Soroban RPC retention window.
 */
/**
 * Every topic the token contract emits, and the set the indexer and live-poll
 * hooks subscribe to against a token contract address.
 *
 * Derived from the `symbol_short!` literals in `contracts/token/src/lib.rs`
 * (its own `EXPECTED_TOPICS` fixture is the source of truth). Kept in sync by
 * `lib/__tests__/trackedEventTopics.test.ts`, which reads the contract source
 * directly — the drift this list had accumulated is exactly what that test
 * now prevents.
 */
export const TRACKED_EVENT_TOPICS = [
  "init",
  "transfer",
  "mint",
  "burn",
  "clawback",
  "freeze",
  "unfreeze",
  "pause",
  "unpause",
  "authorize",
  "rev_auth",
  "revoked",
  "upgrade",
  "approve",
  "set_max_b",
  "set_cnode",
  "prop_adm",
  "cncl_adm",
  "set_admin",
  "upd_uri",
  "set_areq",
  "rvk_rvc",
] as const;

/**
 * Every topic the vesting contract emits.
 *
 * Note `init`, `pause`, `unpause`, `prop_adm`, `revoked` and `upgrade` are
 * emitted by *both* contracts with identical topic tuples, so topic-only
 * filtering cannot tell them apart. Decoding is keyed on the emitting
 * contract as well as the topic — see `decodeActivityEvent`.
 */
export const TRACKED_VESTING_EVENT_TOPICS = [
  "init",
  "prop_adm",
  "acc_adm",
  "create",
  "batch",
  "release",
  "revoke",
  "clf_ext",
  "pause",
  "unpause",
  "prune",
  "upgrade",
  "revoked",
] as const;

/** Which contract emitted an event. Disambiguates the shared topic names. */
export type ActivitySource = "token" | "vesting";

type TrackedTopic = (typeof TRACKED_EVENT_TOPICS)[number];
type TrackedVestingTopic = (typeof TRACKED_VESTING_EVENT_TOPICS)[number];

function isTrackedTopic(s: string): s is TrackedTopic {
  return (TRACKED_EVENT_TOPICS as readonly string[]).includes(s);
}

function isTrackedVestingTopic(s: string): s is TrackedVestingTopic {
  return (TRACKED_VESTING_EVENT_TOPICS as readonly string[]).includes(s);
}

/**
 * Resolve a raw topic name to an activity type, namespacing vesting events.
 *
 * `init` from the token contract and `init` from the vesting contract are
 * different activities to a reader, so the vesting ones become
 * `vesting:init`, `vesting:pause`, and so on. That keeps a single string key
 * for the feed to switch on while leaving the six shared names unambiguous.
 */
export function resolveActivityType(
  topic: string,
  source: ActivitySource,
): TokenActivityType {
  if (source === "vesting") {
    return isTrackedVestingTopic(topic) ? `vesting:${topic}` : "other";
  }
  return isTrackedTopic(topic) ? topic : "other";
}

/**
 * Decode a single raw event into a partial TokenActivityInfo.
 * Returns null for topics we don't recognise.
 */
export function decodeActivityEvent(
  topicStrings: string[],
  value: string | undefined,
  meta: { id: string; txHash: string; ledger: number; timestamp: string },
  source: ActivitySource = "token",
): TokenActivityInfo | null {
  if (topicStrings.length === 0) return null;
  const topic0 = toScVal(topicStrings[0]);
  if (!topic0) return null;
  const typePath = decodeString(topic0);

  const base: TokenActivityInfo = {
    id: meta.id,
    pagingToken: meta.id,
    ledger: meta.ledger,
    type: "other",
    amount: "-",
    from: "-",
    to: "-",
    txHash: meta.txHash,
    timestamp: meta.timestamp,
  };

  // Keyed on (contract, topic): the same topic name means different things
  // depending on which contract emitted it.
  base.type = resolveActivityType(typePath, source);

  const data = toScVal(value);

  if (source === "vesting") {
    decodeVestingPayload(base, typePath, topicStrings, data);
    return base;
  }

  switch (typePath) {
    case "mint": {
      if (data) base.amount = decodeI128(data);
      // SEP-41: topics are ("mint", admin, to)
      if (topicStrings.length > 2) {
        const adminVal = toScVal(topicStrings[1]);
        const toVal = toScVal(topicStrings[2]);
        if (adminVal) base.from = decodeAddress(adminVal);
        if (toVal) base.to = decodeAddress(toVal);
      }
      break;
    }
    case "burn": {
      if (data) base.amount = decodeI128(data);
      if (topicStrings.length > 1) {
        const fromVal = toScVal(topicStrings[1]);
        if (fromVal) base.from = decodeAddress(fromVal);
      }
      break;
    }
    case "clawback": {
      if (data) base.amount = decodeI128(data);
      // SEP-41: topics are ("clawback", admin, from)
      if (topicStrings.length > 2) {
        const adminVal = toScVal(topicStrings[1]);
        const fromVal = toScVal(topicStrings[2]);
        if (adminVal) base.to = decodeAddress(adminVal);
        if (fromVal) base.from = decodeAddress(fromVal);
      }
      break;
    }
    case "transfer": {
      if (data) base.amount = decodeI128(data);
      if (topicStrings.length > 2) {
        const fromVal = toScVal(topicStrings[1]);
        const toVal = toScVal(topicStrings[2]);
        if (fromVal) base.from = decodeAddress(fromVal);
        if (toVal) base.to = decodeAddress(toVal);
      }
      break;
    }
    case "freeze":
    case "unfreeze":
    case "authorize":
    case "rev_auth": {
      // topic[1] = account address being acted on
      if (topicStrings.length > 1) {
        const addrVal = toScVal(topicStrings[1]);
        if (addrVal) base.subject = decodeAddress(addrVal);
      }
      break;
    }
    case "prop_adm": {
      // topics are ("prop_adm", current_admin, new_admin)
      if (topicStrings.length > 2) {
        const currentVal = toScVal(topicStrings[1]);
        const newVal = toScVal(topicStrings[2]);
        if (currentVal) base.from = decodeAddress(currentVal);
        if (newVal) base.to = decodeAddress(newVal);
      }
      break;
    }
    case "set_admin": {
      // topics are ("set_admin", old_admin, new_admin)
      if (topicStrings.length > 2) {
        const oldVal = toScVal(topicStrings[1]);
        const newVal = toScVal(topicStrings[2]);
        if (oldVal) base.from = decodeAddress(oldVal);
        if (newVal) base.to = decodeAddress(newVal);
      }
      break;
    }
    case "revoked":
    case "pause":
    case "unpause":
    case "upgrade":
    case "init":
    case "approve":
    case "set_max_b":
    case "set_cnode":
    case "cncl_adm":
    case "upd_uri":
    case "set_areq":
    case "rvk_rvc":
      // No address payload or special handling needed for activity feed
      break;
    default:
      break;
  }

  return base;
}

/**
 * Fill in the address and amount columns for a vesting-contract event.
 *
 * Topic and data shapes come from `docs/events.json`, the checked-in fixture
 * the vesting contract's own drift test asserts against.
 */
function decodeVestingPayload(
  base: TokenActivityInfo,
  topic: string,
  topicStrings: string[],
  data: StellarSdk.xdr.ScVal | null,
): void {
  /** `create`, `release`, `revoke` and `clf_ext` carry the recipient at topic 1. */
  const readRecipient = () => {
    if (topicStrings.length > 1) {
      const recipient = toScVal(topicStrings[1]);
      if (recipient) base.subject = decodeAddress(recipient);
    }
  };

  switch (topic) {
    case "create":
    case "release": {
      readRecipient();
      if (data) base.amount = decodeI128(data);
      break;
    }
    case "revoke": {
      // data is (releasable, unvested); the releasable half is what the
      // recipient still receives, so that is the figure worth showing.
      readRecipient();
      if (data) base.amount = decodeFirstI128OfTuple(data);
      break;
    }
    case "clf_ext": {
      // data is (old_cliff, new_cliff) — ledger numbers, not an amount.
      readRecipient();
      break;
    }
    case "batch": {
      // data is (created_count, total_amount).
      if (data) base.amount = decodeSecondI128OfTuple(data);
      break;
    }
    case "prune": {
      if (data) base.subject = decodeAddress(data);
      break;
    }
    case "prop_adm":
    case "acc_adm": {
      if (data) base.to = decodeAddress(data);
      break;
    }
    default:
      // init, pause, unpause, upgrade, revoked carry nothing the feed shows.
      break;
  }
}

/** First element of an ScVal tuple, as an i128 string; "-" if not shaped so. */
function decodeFirstI128OfTuple(data: StellarSdk.xdr.ScVal): string {
  return decodeI128OfTupleAt(data, 0);
}

/** Second element of an ScVal tuple, as an i128 string; "-" if not shaped so. */
function decodeSecondI128OfTuple(data: StellarSdk.xdr.ScVal): string {
  return decodeI128OfTupleAt(data, 1);
}

function decodeI128OfTupleAt(data: StellarSdk.xdr.ScVal, index: number): string {
  try {
    const vec = data.vec();
    const element = vec?.[index];
    return element ? decodeI128(element) : "-";
  } catch {
    return "-";
  }
}

export async function fetchTransactionHistory(
  contractId: string,
  config: NetworkConfig,
  options: { cursor?: string; limit?: number } = {},
): Promise<{
  items: TransactionItem[];
  nextCursor: string | null;
  window: HistoryWindow;
}> {
  const { cursor, limit = 200 } = options;

  // Do NOT pass TRACKED_EVENT_TOPICS as RPC topic filters.
  // Each topic would become a separate EventFilter, which breaks global ledger
  // ordering, splits the page budget, and can exceed the RPC filter cap.
  // Client-side filtering by typePath (below) already discards non-transfer
  // events from the result set. See: github.com/soropad/launchpad/issues/472
  const { events, nextCursor } = await fetchIndexedEvents(contractId, config, {
    cursor,
    limit,
  });

  const items: TransactionItem[] = [];

  for (const event of events) {
    // Skip events that cannot be placed in chain order or lack a timestamp.
    if (event.ledger === null || event.timestamp === null) continue;

    const topic0 = toScVal(event.topic[0]);
    if (!topic0) continue;

    const typePath = decodeString(topic0);
    // fetchTransactionHistory returns only the classic token-transfer types
    if (
      typePath !== "mint" &&
      typePath !== "burn" &&
      typePath !== "clawback" &&
      typePath !== "transfer"
    ) {
      continue;
    }

    const data = toScVal(event.value);
    if (!data) continue;

    const item: Partial<TransactionItem> = {
      type: typePath as TransactionItem["type"],
      ledger: event.ledger,
      timestamp: Math.floor(Date.parse(event.timestamp) / 1000) || 0,
      id: event.id || `${event.tx_hash}-${event.ledger}`,
    };

    item.amount = decodeI128(data);

    if (typePath === "mint" && event.topic.length > 2) {
      // SEP-41: topics are ("mint", admin, to)
      const admin = toScVal(event.topic[1]);
      const to = toScVal(event.topic[2]);
      if (admin) item.from = decodeAddress(admin);
      if (to) item.to = decodeAddress(to);
    } else if (typePath === "burn" && event.topic.length > 1) {
      const from = toScVal(event.topic[1]);
      if (from) item.from = decodeAddress(from);
    } else if (typePath === "clawback" && event.topic.length > 2) {
      // SEP-41: topics are ("clawback", admin, from)
      const admin = toScVal(event.topic[1]);
      const from = toScVal(event.topic[2]);
      if (admin) item.to = decodeAddress(admin);
      if (from) item.from = decodeAddress(from);
    } else if (typePath === "transfer" && event.topic.length > 2) {
      const from = toScVal(event.topic[1]);
      const to = toScVal(event.topic[2]);
      if (from) item.from = decodeAddress(from);
      if (to) item.to = decodeAddress(to);
    }

    items.push(item as TransactionItem);
  }

  // The RPC fallback anchors its no-cursor scan ~1000 ledgers behind the head
  // (see indexer.ts). Surface the oldest ledger we actually saw so callers can
  // tell the user the window is truncated rather than presenting it as the
  // token's full history. A cursor means the caller is paging through an
  // already-established window, so we only compute this on the first page.
  const window = computeHistoryWindow(events, cursor);

  return { items: items.reverse(), nextCursor, window };
}

function computeHistoryWindow(
  events: { ledger: number | null }[],
  cursor: string | undefined,
): HistoryWindow {
  if (cursor) {
    return { startLedger: null, truncated: false };
  }
  if (events.length === 0) {
    return { startLedger: null, truncated: false };
  }
  // Exclude events with no known ledger from the window calculation.
  const knownLedgers = events
    .map((e) => e.ledger)
    .filter((l): l is number => l !== null);
  if (knownLedgers.length === 0) {
    return { startLedger: null, truncated: false };
  }
  const startLedger = knownLedgers.reduce(
    (min, l) => (l < min ? l : min),
    knownLedgers[0],
  );
  return { startLedger, truncated: true };
}

export type TokenActivityType =
  | (typeof TRACKED_EVENT_TOPICS)[number]
  | `vesting:${(typeof TRACKED_VESTING_EVENT_TOPICS)[number]}`
  | "other";

export interface TokenActivityInfo {
  id: string;
  pagingToken: string;
  ledger?: number;
  type: TokenActivityType;
  amount: string;
  from: string;
  to: string;
  /** address or subject involved in admin/compliance events */
  subject?: string;
  timestamp: string;
  txHash: string;
}

/**
 * Fetch token activity operations for a given account or contract ID.
 * Parses classic `payment` and Soroban `invoke_host_function` operations.
 */
export async function fetchAccountOperations(
  accountId: string,
  config: NetworkConfig,
  cursor?: string,
  limit = 10,
): Promise<{
  records: TokenActivityInfo[];
  nextCursor: string | null;
  window: HistoryWindow;
}> {
  try {
    // For contract IDs, use indexer events instead of Horizon.
    if (accountId.startsWith("C")) {
      // Do NOT pass TRACKED_EVENT_TOPICS as RPC topic filters.
      // Each topic would become a separate EventFilter, which breaks global
      // ledger ordering, splits the page budget, and can exceed the RPC
      // filter cap. decodeActivityEvent already filters by topic client-side.
      // See: github.com/soropad/launchpad/issues/472
      const pageSize = Math.min(limit, 200);

      const { events, nextCursor: nextIndexerCursor } =
        await fetchIndexedEvents(accountId, config, {
          limit: pageSize,
          cursor: cursor ?? undefined,
        });

      const records: TokenActivityInfo[] = [];

      for (const event of events) {
        // Skip events that cannot be placed in chain order or lack a timestamp.
        if (event.ledger === null || event.timestamp === null) continue;

        const decoded = decodeActivityEvent(
          // IndexedEvent.topic is unknown[]; the decoder re-parses each entry
          // as an XDR-encoded string, so normalize before handing it over.
          event.topic.map((t) => String(t)),
          typeof event.value === "string" ? event.value : undefined,
          {
            id: event.id || `${event.tx_hash}-${event.ledger}`,
            txHash: event.tx_hash,
            ledger: event.ledger,
            timestamp: event.timestamp,
          },
        );
        if (decoded) records.push(decoded);
      }

      return {
        records,
        nextCursor: nextIndexerCursor,
        window: computeHistoryWindow(events, cursor),
      };
    }

    const horizon = new StellarSdk.Horizon.Server(getHorizonUrl());

    // Horizon's .forAccount() only accepts Ed25519 public keys (starting with G).
    if (!accountId.startsWith("G") && !accountId.startsWith("M")) {
      return { records: [], nextCursor: null, window: { startLedger: null, truncated: false } };
    }

    let callBuilder = horizon
      .operations()
      .forAccount(accountId)
      .limit(limit)
      .order("desc");
    if (cursor) {
      callBuilder = callBuilder.cursor(cursor);
    }

    const response = await callBuilder.call();

    // Extract paging token for the next page, from the last record fetched
    // (since order is desc, the last record in this array is the oldest).
    const nextCursor =
      response.records.length > 0
        ? response.records[response.records.length - 1].paging_token
        : null;

    const parsed: TokenActivityInfo[] = [];

    for (const record of response.records) {
      // Classic Native/Asset Payments
      if (record.type === "payment") {
        const r = record as unknown as Record<string, unknown>; // Horizon.ServerApi.PaymentOperationRecord
        // Native mints aren't strictly 'payment' but for asset payments:
        const isMint = r.from === r.asset_issuer;
        const isBurn = r.to === r.asset_issuer; // simplified burn heuristic
        let typeInfo: "mint" | "transfer" | "burn" = "transfer";
        if (isMint) typeInfo = "mint";
        else if (isBurn) typeInfo = "burn";

        parsed.push({
          id: record.id,
          pagingToken: record.paging_token,
          type: typeInfo,
          amount: typeof r.amount === "string" ? r.amount : "0",
          from: typeof r.from === "string" ? r.from : "Unknown",
          to: typeof r.to === "string" ? r.to : "Unknown",
          timestamp: record.created_at,
          txHash: record.transaction_hash,
        });
      }
      // Soroban Contract Invokes
      else if (record.type === "invoke_host_function") {
        const r = record as unknown as Record<string, unknown>;
        // Check for balance changes (requires Soroban RPC / Horizon with Soroban ingestion)
        // Note: Soroban Horizon responses might include `asset_balance_changes` if it was a token transfer
        const balChanges = r.asset_balance_changes;
        if (Array.isArray(balChanges) && balChanges.length > 0) {
          // Find the transfer or mint that is most relevant.
          // This is a simplified heuristic. We pick the first transfer for now.
          const transfer = balChanges.find((c: unknown) => {
            if (!c || typeof c !== "object") return false;
            const cast = c as Record<string, unknown>;
            return (
              cast.type === "transfer" ||
              cast.type === "mint" ||
              cast.type === "burn"
            );
          }) as Record<string, unknown> | undefined;

          if (transfer) {
            let actType: "mint" | "transfer" | "burn" = "transfer";
            if (transfer.from === r.source_account && transfer.type === "mint")
              actType = "mint"; // very rough heuristic, actual type is in transfer.type
            if (transfer.type === "mint" || transfer.type === "burn")
              actType = transfer.type as "mint" | "burn";

            parsed.push({
              id: record.id,
              pagingToken: record.paging_token,
              type: actType,
              amount:
                typeof transfer.amount === "string" ? transfer.amount : "0",
              from:
                typeof transfer.from === "string"
                  ? transfer.from
                  : typeof r.source_account === "string"
                    ? r.source_account
                    : "Unknown",
              to: typeof transfer.to === "string" ? transfer.to : "Unknown",
              timestamp: record.created_at,
              txHash: record.transaction_hash,
            });
            continue; // parsed successfully via balance changes
          }
        }

        // If we couldn't parse balance changes, mark as generic
        parsed.push({
          id: record.id,
          pagingToken: record.paging_token,
          type: "other",
          amount: "-",
          from:
            typeof r.source_account === "string" ? r.source_account : "Unknown",
          to: "-",
          timestamp: record.created_at,
          txHash: record.transaction_hash,
        });
      }
    }

    // Filter out "other" if we only want token activity, but keeping it helps visibility
    const filtered = parsed.filter((p) => p.type !== "other");

    return {
      records: filtered.length > 0 ? filtered : parsed,
      nextCursor,
      window: { startLedger: null, truncated: false },
    };
  } catch (error) {
    console.error("Error fetching account operations from Horizon:", error);
    return { records: [], nextCursor: null, window: { startLedger: null, truncated: false } };
  }
}

export interface AccountBalance {
  assetType: "native" | "credit_alphanum4" | "credit_alphanum12";
  assetCode: string;
  assetIssuer: string;
  balance: string;
}

/**
 * Fetch all balances for a Stellar account from Horizon.
 */
export async function fetchAccountBalances(
  publicKey: string,
  config: NetworkConfig,
): Promise<AccountBalance[]> {
  const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);
  const account = await horizon.loadAccount(publicKey);

  return account.balances.map((bal) => {
    if (bal.asset_type === "native") {
      return {
        assetType: "native" as const,
        assetCode: "XLM",
        assetIssuer: "",
        balance: bal.balance,
      };
    }
    const b = bal as unknown as {
      asset_type: string;
      asset_code: string;
      asset_issuer: string;
      balance: string;
    };
    return {
      assetType: b.asset_type as AccountBalance["assetType"],
      assetCode: b.asset_code,
      assetIssuer: b.asset_issuer,
      balance: b.balance,
    };
  });
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

export function formatTokenAmount(raw: string, decimals: number): string {
  if (raw === "N/A") return raw;
  const num = BigInt(raw);
  const divisor = BigInt(10) ** BigInt(decimals);
  const whole = num / divisor;
  const frac = num % divisor;

  if (frac === BigInt(0)) return whole.toLocaleString();

  const fracStr = frac.toString().padStart(decimals, "0").replace(/0+$/, "");
  return `${whole.toLocaleString()}.${fracStr}`;
}

export function parseTokenAmount(amount: string, decimals: number): bigint {
  const trimmed = amount.trim().replace(/,/g, "");
  if (!trimmed) {
    throw new Error("Amount is required");
  }

  if (trimmed.startsWith("-")) {
    throw new Error("Amount must be positive");
  }

  const [wholeRaw, fracRaw = ""] = trimmed.split(".");
  if (!/^\d+$/.test(wholeRaw || "0")) {
    throw new Error("Invalid amount");
  }
  if (fracRaw && !/^\d+$/.test(fracRaw)) {
    throw new Error("Invalid amount");
  }

  if (fracRaw.length > decimals) {
    throw new Error(`Too many decimal places (max ${decimals})`);
  }

  const whole = BigInt(wholeRaw || "0");
  const fracPadded = (fracRaw || "").padEnd(decimals, "0");
  const frac = fracPadded ? BigInt(fracPadded) : BigInt(0);
  const scale = BigInt(10) ** BigInt(decimals);
  return whole * scale + frac;
}

export async function fetchTokenDecimals(
  tokenContractId: string,
  config: NetworkConfig,
): Promise<number> {
  const result = await simulateCall(tokenContractId, "decimals", config);
  return decodeU32(result);
}

export function truncateAddress(addr: string, chars = 4): string {
  if (addr.length <= chars * 2 + 3) return addr;
  return `${addr.slice(0, chars + 1)}...${addr.slice(-chars)}`;
}

// ---------------------------------------------------------------------------
// Stellar Expert link helpers
// ---------------------------------------------------------------------------

/**
 * Generate a Stellar Expert URL for various blockchain entities.
 *
 * @param type - The type of entity: 'account', 'contract', or 'tx'
 * @param identifier - The public key, contract ID, or transaction hash
 * @param network - The network type ('testnet' or 'mainnet')
 * @returns The full Stellar Expert URL
 */
export function getStellarExpertUrl(
  type: "account" | "contract" | "tx",
  identifier: string,
  network: "testnet" | "mainnet" = "testnet",
): string {
  const baseUrl = "https://stellar.expert/explorer";
  return `${baseUrl}/${network}/${type}/${identifier}`;
}

// ---------------------------------------------------------------------------
// Supply breakdown helpers
// ---------------------------------------------------------------------------

export interface SupplyBreakdown {
  circulating: number;
  locked: number;
  burned: number;
  burnedAvailable: boolean;
  total: number;
}

/**
 * Calculate supply breakdown for a token.
 *
 * @param tokenContractId - The token contract ID
 * @param vestingContractId - Optional vesting contract ID to calculate locked supply
 * @returns Supply breakdown with circulating, locked, and burned amounts
 */
export async function fetchSupplyBreakdown(
  tokenContractId: string,
  config: NetworkConfig,
  vestingContractId?: string,
): Promise<SupplyBreakdown> {
  try {
    // Fetch total supply from token contract
    const totalSupplyVal = await simulateCall(
      tokenContractId,
      "total_supply",
      config,
    );
    const totalSupply = Number(decodeI128(totalSupplyVal));

    // Fetch total burned from token contract (tracked explicitly on-chain)
    let burnedSupply = 0;
    let burnedAvailable = false;
    try {
      const burnedVal = await simulateCall(
        tokenContractId,
        "total_burned",
        config,
      );
      burnedSupply = Number(decodeI128(burnedVal));
      burnedAvailable = true;
    } catch {
      burnedSupply = 0;
      burnedAvailable = false;
    }

    // Locked supply is modeled as the token balance held by the vesting
    // contract (or contracts). When a vesting schedule is created, tokens
    // are transferred to the vesting contract address and remain there
    // until released.
    let lockedSupply = 0;
    if (vestingContractId) {
      try {
        const vestingAddr = new StellarSdk.Address(vestingContractId).toScVal();
        const lockedVal = await simulateCall(
          tokenContractId,
          "balance",
          config,
          [vestingAddr],
        );
        lockedSupply = Number(decodeI128(lockedVal));
      } catch {
        // If the vesting contract or balance call fails, fall back to 0
        lockedSupply = 0;
      }
    }

    const circulatingSupply = totalSupply - lockedSupply - burnedSupply;

    return {
      circulating: circulatingSupply,
      locked: lockedSupply,
      burned: burnedSupply,
      burnedAvailable,
      total: totalSupply,
    };
  } catch (error) {
    console.error("[fetchSupplyBreakdown] Error:", error);
    throw new Error("Failed to fetch supply breakdown");
  }
}

// Transaction building and submission
// ---------------------------------------------------------------------------

/**
 * Build a transaction XDR for revoking a vesting schedule.
 * Returns the unsigned transaction XDR string.
 */
export async function buildRevokeTransaction(
  vestingContractId: string,
  recipientAddress: string,
  sourcePublicKey: string,
  config: NetworkConfig,
  scheduleIndex?: number,
): Promise<string> {
  const contract = new StellarSdk.Contract(vestingContractId);
  const resolved = await resolveVestingScheduleIndex({
    vestingContractId,
    recipient: recipientAddress,
    config,
    scheduleIndex,
  });
  const recipientScVal = new StellarSdk.Address(recipientAddress).toScVal();
  const indexScVal = toU32ScVal(resolved.scheduleIndex);

  // Get source account
  const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);
  const sourceAccount = await horizon.loadAccount(sourcePublicKey);

  // Build transaction
  const tx = new StellarSdk.TransactionBuilder(sourceAccount, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: config.passphrase,
  })
    .addOperation(contract.call("revoke", recipientScVal, indexScVal))
    .setTimeout(30)
    .build();

  // Simulate to get resource fees
  const assembled = await simulateAndAssembleTransaction(tx, config);
  return assembled.build().toXDR();
}

/**
 * Build a transaction XDR for SEP-41 approve (grant/revoke allowance).
 * Returns the unsigned transaction XDR string.
 */
export async function buildApproveTransaction(params: {
  tokenContractId: string;
  ownerAddress: string;
  spenderAddress: string;
  amount: bigint;
  expirationLedger: number;
  config: NetworkConfig;
}): Promise<string> {
  const {
    tokenContractId,
    ownerAddress,
    spenderAddress,
    amount,
    expirationLedger,
    config,
  } = params;

  const contract = new StellarSdk.Contract(tokenContractId);
  const ownerScVal = new StellarSdk.Address(ownerAddress).toScVal();
  const spenderScVal = new StellarSdk.Address(spenderAddress).toScVal();
  const amountScVal = StellarSdk.nativeToScVal(BigInt(amount), {
    type: "i128",
  });
  const expirationScVal = StellarSdk.nativeToScVal(BigInt(expirationLedger), {
    type: "u32",
  });

  const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);
  const sourceAccount = await horizon.loadAccount(ownerAddress);

  const tx = new StellarSdk.TransactionBuilder(sourceAccount, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: config.passphrase,
  })
    .addOperation(
      contract.call(
        "approve",
        ownerScVal,
        spenderScVal,
        amountScVal,
        expirationScVal,
      ),
    )
    .setTimeout(30)
    .build();

  const assembled = await simulateAndAssembleTransaction(tx, config);
  return assembled.build().toXDR();
}

/**
 * Build a transaction XDR that revokes an allowance on a SEP-41 token.
 *
 * Delegates argument construction to `buildRevokeAllowanceArgs` so the
 * preflight simulation and the real submission use identical arguments.
 * See that function and `contracts/token/src/lib.rs` for the rationale.
 */
export async function buildRevokeAllowanceTransaction(params: {
  tokenContractId: string;
  ownerAddress: string;
  spenderAddress: string;
  config: NetworkConfig;
}): Promise<string> {
  const { tokenContractId, ownerAddress, spenderAddress, config } = params;

  const contract = new StellarSdk.Contract(tokenContractId);
  const args = buildRevokeAllowanceArgs(ownerAddress, spenderAddress);

  const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);
  const sourceAccount = await horizon.loadAccount(ownerAddress);

  const tx = new StellarSdk.TransactionBuilder(sourceAccount, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: config.passphrase,
  })
    .addOperation(contract.call("approve", ...args))
    .setTimeout(30)
    .build();

  const assembled = await simulateAndAssembleTransaction(tx, config);
  return assembled.build().toXDR();
}

/**
 * Build a transaction XDR for SEP-41 transfer_from.
 * Returns the unsigned transaction XDR string.
 */
export async function buildTransferFromTransaction(params: {
  tokenContractId: string;
  spenderAddress: string;
  fromAddress: string;
  toAddress: string;
  amount: bigint;
  config: NetworkConfig;
}): Promise<string> {
  const {
    tokenContractId,
    spenderAddress,
    fromAddress,
    toAddress,
    amount,
    config,
  } = params;

  const contract = new StellarSdk.Contract(tokenContractId);
  const spenderScVal = new StellarSdk.Address(spenderAddress).toScVal();
  const fromScVal = new StellarSdk.Address(fromAddress).toScVal();
  const toScVal = new StellarSdk.Address(toAddress).toScVal();
  const amountScVal = StellarSdk.nativeToScVal(BigInt(amount), {
    type: "i128",
  });

  const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);
  const sourceAccount = await horizon.loadAccount(spenderAddress);

  const tx = new StellarSdk.TransactionBuilder(sourceAccount, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: config.passphrase,
  })
    .addOperation(
      contract.call(
        "transfer_from",
        spenderScVal,
        fromScVal,
        toScVal,
        amountScVal,
      ),
    )
    .setTimeout(30)
    .build();

  const assembled = await simulateAndAssembleTransaction(tx, config);
  return assembled.build().toXDR();
}

/**
 * Build a transaction XDR for burning tokens.
 * Returns the unsigned transaction XDR string.
 */
export async function buildBurnTransaction(
  tokenContractId: string,
  fromAddress: string,
  amount: string,
  decimals: number,
  config: NetworkConfig,
): Promise<string> {
  const contract = new StellarSdk.Contract(tokenContractId);
  const fromScVal = new StellarSdk.Address(fromAddress).toScVal();
  const rawAmount = parseTokenAmount(amount, decimals);
  const amountScVal = StellarSdk.xdr.ScVal.scvI128(
    new StellarSdk.xdr.Int128Parts({
      hi: StellarSdk.xdr.Int64.fromString((rawAmount >> BigInt(64)).toString()),
      lo: StellarSdk.xdr.Uint64.fromString(
        (rawAmount & ((BigInt(1) << BigInt(64)) - BigInt(1))).toString(),
      ),
    }),
  );

  const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
  const horizon = new StellarSdk.Horizon.Server(config.horizonUrl);

  const sourceAccount = await horizon.loadAccount(fromAddress);

  const tx = new StellarSdk.TransactionBuilder(sourceAccount, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: config.passphrase,
  })
    .addOperation(contract.call("burn_self", fromScVal, amountScVal))
    .setTimeout(30)
    .build();

  const simulated = await rpc.simulateTransaction(tx);

  if (StellarSdk.rpc.Api.isSimulationError(simulated)) {
    throw new Error(`Simulation failed: ${simulated.error}`);
  }

  if (!StellarSdk.rpc.Api.isSimulationSuccess(simulated)) {
    throw new Error("Transaction simulation was not successful");
  }

  const assembled = StellarSdk.rpc.assembleTransaction(tx, simulated);
  return assembled.build().toXDR();
}

/**
 * Submit a signed transaction XDR to the network.
 * Returns the transaction hash on success.
 */
export async function submitTransaction(
  signedXdr: string,
  config: NetworkConfig,
): Promise<string> {
  return wrapRpcCall(
    async () => {
      const tx = StellarSdk.TransactionBuilder.fromXDR(
        signedXdr,
        config.passphrase,
      );

      const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
      const result = await rpc.sendTransaction(tx as StellarSdk.Transaction);

      if (result.status === "ERROR") {
        throw new Error(
          `Transaction failed: ${result.errorResult?.toXDR("base64")}`,
        );
      }

      // Poll for transaction result
      let getResponse = await rpc.getTransaction(result.hash);
      let attempts = 0;
      const maxAttempts = 30;

      while (getResponse.status === "NOT_FOUND" && attempts < maxAttempts) {
        await new Promise((resolve) => setTimeout(resolve, 1000));
        getResponse = await rpc.getTransaction(result.hash);
        attempts++;
      }

      if (getResponse.status === "NOT_FOUND") {
        throw new Error("Transaction not found after polling");
      }

      if (getResponse.status === "FAILED") {
        throw new Error(
          `Transaction failed: ${getResponse.resultXdr?.toXDR("base64")}`,
        );
      }

      return result.hash;
    },
    { operation: "Submit transaction" },
  );
}

// ---------------------------------------------------------------------------
// Launch attestation
// ---------------------------------------------------------------------------

/**
 * The four on-chain facts a deployer needs before signing.
 *
 * Everything here is read directly from the chain and compared against the
 * audited manifest — no SoroPad API involved. A user can reproduce this
 * independently given only the factory address and their salt.
 */
export interface LaunchAttestation {
  /** Current WASM hash of the factory contract itself (hex). */
  factoryHash: string | null;
  /** WASM hash the factory will use for the token it is about to deploy (hex). */
  tokenWasmHash: string | null;
  /**
   * Whether both hashes match the audited manifest.
   *
   * - `"match"` — both hashes present in manifest and equal to on-chain values.
   * - `"mismatch"` — at least one hash differs from the manifest (fail-closed).
   * - `"unknown"` — manifest could not be fetched or has no entry for this
   *   network; the check is inconclusive, not a hard failure.
   */
  manifestMatch: "match" | "mismatch" | "unknown";
  /** Deterministic token address that `deploy_token(deployer, salt)` will produce. */
  address: string | null;
  /** Non-fatal notices the user should see (e.g. manifest entries are empty). */
  warnings: string[];
}

/**
 * Resolve all four attestation facts for an imminent `deploy_token` call.
 *
 * Fetches the factory's own WASM hash, reads the token WASM hash the factory
 * currently points at (via `get_token_wasm_hash`), computes the deterministic
 * token address from `deployer` + `salt`, and cross-checks both hashes against
 * the audited manifest.
 *
 * Designed to run during Step 4 (Review) before the user signs — all reads
 * are view-only simulations, nothing is submitted.
 */
export async function resolveLaunchAttestation(
  factoryAddress: string,
  deployer: string,
  salt: Uint8Array,
  config: NetworkConfig,
): Promise<LaunchAttestation> {
  const warnings: string[] = [];

  // ── 1. Read factory WASM hash and token WASM hash in parallel ────────
  const [factoryHash, tokenWasmHashResult, manifest] = await Promise.allSettled([
    getContractWasmHash(factoryAddress, config),
    simulateCall(factoryAddress, "get_token_wasm_hash", config),
    fetchWasmManifest(),
  ]);

  const resolvedFactoryHash =
    factoryHash.status === "fulfilled" ? factoryHash.value : null;

  let resolvedTokenWasmHash: string | null = null;
  if (tokenWasmHashResult.status === "fulfilled") {
    try {
      // get_token_wasm_hash returns BytesN<32>
      const rawBytes = tokenWasmHashResult.value.bytes() as Buffer;
      resolvedTokenWasmHash = Buffer.from(rawBytes).toString("hex");
    } catch {
      // Could not decode the return value
    }
  }

  // ── 2. Compute deterministic token address via factory view call ─────
  // The factory exposes `get_deployment_address(deployer, salt) -> Address`,
  // which mirrors the host's deterministic derivation without submitting
  // anything. This is the canonical way to get the address client-side.
  let address: string | null = null;
  try {
    const saltScVal = StellarSdk.nativeToScVal(Buffer.from(salt));
    const deployerScVal = new StellarSdk.Address(deployer).toScVal();
    const retval = await simulateCall(
      factoryAddress,
      "get_deployment_address",
      config,
      [deployerScVal, saltScVal],
    );
    address = StellarSdk.Address.fromScVal(retval).toString();
  } catch {
    // View call failed — factory might not be deployed yet or RPC unreachable
    address = null;
  }

  // ── 3. Compare against manifest ──────────────────────────────────────
  const mfst = manifest.status === "fulfilled" ? manifest.value : null;

  let manifestMatch: LaunchAttestation["manifestMatch"] = "unknown";

  if (!mfst) {
    warnings.push(
      "The WASM manifest could not be fetched. Hash verification is unavailable.",
    );
  } else {
    const networkEntry = mfst.factory?.deployments?.[config.network];
    const manifestFactoryHash = networkEntry?.wasm_hash ?? null;
    const manifestTokenHash =
      mfst.token?.versions?.[mfst.token?.latest ?? ""]?.wasm_hash ?? null;

    const factoryHashKnown = !!manifestFactoryHash;
    const tokenHashKnown = !!manifestTokenHash;

    if (!factoryHashKnown && !tokenHashKnown) {
      warnings.push(
        "The manifest has no reference hashes for this network. Verification is inconclusive.",
      );
      manifestMatch = "unknown";
    } else {
      const factoryOk =
        !factoryHashKnown ||
        !resolvedFactoryHash ||
        resolvedFactoryHash.toLowerCase() === manifestFactoryHash!.toLowerCase();

      const tokenOk =
        !tokenHashKnown ||
        !resolvedTokenWasmHash ||
        resolvedTokenWasmHash.toLowerCase() === manifestTokenHash!.toLowerCase();

      if (factoryOk && tokenOk) {
        manifestMatch = "match";
      } else {
        manifestMatch = "mismatch";
        if (!factoryOk) {
          warnings.push(
            `Factory WASM mismatch — on-chain: ${resolvedFactoryHash?.slice(0, 12)}…, manifest: ${manifestFactoryHash?.slice(0, 12)}…`,
          );
        }
        if (!tokenOk) {
          warnings.push(
            `Token WASM mismatch — on-chain: ${resolvedTokenWasmHash?.slice(0, 12)}…, manifest: ${manifestTokenHash?.slice(0, 12)}…`,
          );
        }
      }

      if (factoryHashKnown && !resolvedFactoryHash) {
        warnings.push("Could not read the factory's on-chain WASM hash.");
      }
      if (tokenHashKnown && !resolvedTokenWasmHash) {
        warnings.push(
          "Could not read the token WASM hash from the factory.",
        );
      }
    }
  }

  return {
    factoryHash: resolvedFactoryHash,
    tokenWasmHash: resolvedTokenWasmHash,
    manifestMatch,
    address,
    warnings,
  };
}
