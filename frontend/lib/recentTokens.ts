import * as StellarSdk from "@stellar/stellar-sdk";
import { type NetworkConfig } from "../types/network";
import { fetchTokenInfo, type TokenInfo } from "./stellar";
import { LEDGERS_PER_DAY } from "./soroban";

export interface RecentToken extends TokenInfo {
  deployedAt: string;
  activityScore: number;
}

interface RpcEvent {
  contractId?: string;
  ledger?: number;
  ledgerClosedAt?: string;
  topic?: string[];
  value?: string;
}

// Fallback lookback when the RPC's actual retention window can't be probed
// (e.g. getHealth() unsupported or unreachable) — the previous fixed window.
const FALLBACK_LOOKBACK_LEDGERS = 17280; // ~24 hours at ~5s per ledger
const LOOKBACK_LEDGERS = LEDGERS_PER_DAY; // ~24 hours at ~5s per ledger
const MAX_CANDIDATES = 20;
const MAX_RESULTS = 12;
const FACTORY_PAGE_SIZE = 100;

async function safeGetEvents(
  getEvents: (req: unknown) => Promise<unknown>,
  request: unknown,
): Promise<RpcEvent[]> {
  try {
    const response = await getEvents(request);
    const obj = (response ?? {}) as { events?: unknown[] };
    return Array.isArray(obj.events) ? (obj.events as RpcEvent[]) : [];
  } catch {
    return [];
  }
}

/**
 * Fetch every contract address registered in the SoroPad factory and return
 * them as a Set for O(1) membership checks.
 *
 * Returns an empty Set when `factoryAddress` is blank or any RPC call fails,
 * so the caller degrades gracefully rather than hard-failing the feed.
 */
export async function fetchFactoryRegistry(
  config: NetworkConfig,
  factoryAddress: string,
): Promise<Set<string>> {
  if (!factoryAddress) return new Set();

  try {
    const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
    // A random keypair is sufficient for read-only simulation — no funded
    // source account is required.
    const account = new StellarSdk.Account(
      StellarSdk.Keypair.random().publicKey(),
      "0",
    );

    // 1. Get the total deployment count.
    const countTx = new StellarSdk.TransactionBuilder(account, {
      fee: StellarSdk.BASE_FEE,
      networkPassphrase: config.passphrase,
    })
      .addOperation(
        new StellarSdk.Contract(factoryAddress).call("get_deployment_count"),
      )
      .setTimeout(30)
      .build();

    const countSim = await rpc.simulateTransaction(countTx);
    if (
      !StellarSdk.rpc.Api.isSimulationSuccess(countSim) ||
      !countSim.result
    ) {
      return new Set();
    }

    const count = Number(
      StellarSdk.scValToNative(countSim.result.retval),
    );
    if (count === 0) return new Set();

    // 2. Page through the registry and collect all deployed addresses.
    const registry = new Set<string>();
    for (let start = 0; start < count; start += FACTORY_PAGE_SIZE) {
      const limit = Math.min(FACTORY_PAGE_SIZE, count - start);
      const pageTx = new StellarSdk.TransactionBuilder(account, {
        fee: StellarSdk.BASE_FEE,
        networkPassphrase: config.passphrase,
      })
        .addOperation(
          new StellarSdk.Contract(factoryAddress).call(
            "get_deployments_paginated",
            StellarSdk.nativeToScVal(start, { type: "u32" }),
            StellarSdk.nativeToScVal(limit, { type: "u32" }),
          ),
        )
        .setTimeout(30)
        .build();

      const pageSim = await rpc.simulateTransaction(pageTx);
      if (
        StellarSdk.rpc.Api.isSimulationSuccess(pageSim) &&
        pageSim.result
      ) {
        const addresses = StellarSdk.scValToNative(
          pageSim.result.retval,
        ) as string[];
        for (const addr of addresses) {
          registry.add(addr);
        }
      }
    }

    return registry;
  } catch {
    return new Set();
  }
}

/**
 * Resolve the oldest ledger the RPC can still serve events for, so the launch
 * feed can widen its lookback to the RPC's actual retention window instead of
 * a self-imposed 24h cutoff. Falls back to `FALLBACK_LOOKBACK_LEDGERS` behind
 * `latestLedger` when `getHealth` is unsupported or unreachable, so a launch
 * feed request never hard-fails on a probe failure.
 */
async function resolveStartLedger(
  rpc: StellarSdk.rpc.Server,
  latestLedger: number,
): Promise<number> {
  try {
    const health = await rpc.getHealth();
    if (typeof health.oldestLedger === "number" && health.oldestLedger > 0) {
      return Math.max(1, health.oldestLedger);
    }
  } catch {
    // getHealth unsupported/unreachable — degrade to the fixed fallback below.
  }
  return Math.max(1, latestLedger - FALLBACK_LOOKBACK_LEDGERS);
}

export async function fetchRecentTokens(
  config: NetworkConfig,
  factoryAddress?: string,
): Promise<RecentToken[]> {
  const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
  const getEvents = (
    rpc as unknown as {
      getEvents?: (req: unknown) => Promise<unknown>;
    }
  ).getEvents;
  if (!getEvents) return [];

  const { sequence: latestLedger } = await rpc.getLatestLedger();
  const startLedger = await resolveStartLedger(rpc, latestLedger);

  const initTopic = StellarSdk.xdr.ScVal.scvSymbol("init").toXDR("base64");
  const initEvents = await safeGetEvents(getEvents, {
    startLedger,
    filters: [{ type: "contract", topics: [[initTopic]] }],
    pagination: { limit: 200 },
  });

  const seen = new Map<string, RpcEvent>();
  for (const evt of initEvents) {
    if (evt.contractId && !seen.has(evt.contractId)) {
      seen.set(evt.contractId, evt);
    }
  }

  // Fetch the factory's on-chain registry so the feed only shows tokens that
  // SoroPad deployed. The unnamespaced "init" topic matches every SEP-41
  // token on the network, so without this filter the widget would display
  // unrelated contracts. We intersect here — before hydrating any contract —
  // so no RPC calls are wasted on non-SoroPad tokens.
  //
  // If `factoryAddress` is not configured, or the registry fetch fails, the
  // Set will be empty and *all* event candidates will be filtered out,
  // returning an empty feed rather than silently showing unrelated tokens.
  const factoryRegistry = await fetchFactoryRegistry(
    config,
    factoryAddress ?? "",
  );

  const filteredSeen = factoryRegistry.size > 0
    ? new Map(
        [...seen.entries()].filter(([contractId]) =>
          factoryRegistry.has(contractId),
        ),
      )
    : new Map<string, RpcEvent>();

  // Sort by ledger descending *before* truncating, so a window with more than
  // MAX_CANDIDATES launches keeps the newest ones rather than whichever
  // MAX_CANDIDATES the RPC happened to return first.
  const candidates = Array.from(filteredSeen.entries())
    .sort(([, a], [, b]) => (b.ledger ?? 0) - (a.ledger ?? 0))
    .slice(0, MAX_CANDIDATES);
  if (candidates.length === 0) return [];

  const tokens: RecentToken[] = [];
  const settled = await Promise.allSettled(
    candidates.map(
      async ([contractId, evt]): Promise<RecentToken> => {
        const info = await fetchTokenInfo(contractId, config);
        return {
          ...info,
          deployedAt: evt.ledgerClosedAt ?? "",
          activityScore: 0,
        };
      },
    ),
  );

  for (const result of settled) {
    if (result.status === "fulfilled") {
      tokens.push(result.value);
    }
  }

  if (tokens.length === 0) return [];

  const ids = tokens.map((t) => t.contractId);
  const scores = new Map<string, number>();

  const batches: string[][] = [];
  for (let i = 0; i < ids.length; i += 5) {
    batches.push(ids.slice(i, i + 5));
  }

  // Batches are independent RPC calls — run them concurrently instead of one
  // round trip at a time, so a full candidate-set refresh is a single wave of
  // requests rather than four serial ones.
  const batchResults = await Promise.all(
    batches.map((batch) =>
      safeGetEvents(getEvents, {
        startLedger,
        filters: [{ type: "contract", contractIds: batch }],
        pagination: { limit: 1000 },
      }),
    ),
  );

  for (const events of batchResults) {
    for (const evt of events) {
      if (evt.contractId) {
        scores.set(evt.contractId, (scores.get(evt.contractId) ?? 0) + 1);
      }
    }
  }

  for (const token of tokens) {
    token.activityScore = scores.get(token.contractId) ?? 0;
  }

  tokens.sort(
    (a, b) =>
      b.activityScore - a.activityScore ||
      b.deployedAt.localeCompare(a.deployedAt),
  );

  return tokens.slice(0, MAX_RESULTS);
}
