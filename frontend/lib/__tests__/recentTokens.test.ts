/**
 * Tests for the landing page's recent-launches feed (issues #411, #470):
 *  - lookback widens to the RPC's real retention window via getHealth(),
 *    and degrades gracefully to the fixed fallback when getHealth fails.
 *  - candidates are sorted by ledger (newest first) before truncation.
 *  - the activity-scoring batches run concurrently, not sequentially.
 *  - only tokens in the factory registry are returned (issue #470).
 */

import * as StellarSdk from "@stellar/stellar-sdk";
import { fetchRecentTokens } from "../recentTokens";
import { fetchTokenInfo } from "../stellar";
import { type NetworkConfig } from "../../types/network";

jest.mock("../stellar", () => ({
  fetchTokenInfo: jest.fn(),
}));

jest.mock("@stellar/stellar-sdk", () => {
  const original = jest.requireActual("@stellar/stellar-sdk");
  return {
    ...original,
    rpc: {
      ...original.rpc,
      Server: jest.fn(),
    },
  };
});

const MOCK_FACTORY_ADDRESS = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFCT";

const mockConfig: NetworkConfig = {
  network: "testnet",
  rpcUrl: "https://soroban-testnet.stellar.org",
  horizonUrl: "https://horizon-testnet.stellar.org",
  passphrase: "Test SDF Network ; September 2015",
};

const mockFetchTokenInfo = fetchTokenInfo as jest.Mock;

function makeInitEvent(contractId: string, ledger: number) {
  return {
    contractId,
    ledger,
    ledgerClosedAt: new Date(2026, 0, 1, 0, 0, ledger).toISOString(),
  };
}

/**
 * Build a simulateTransaction mock that answers factory registry calls.
 *
 * `fetchFactoryRegistry` always calls the factory in the same order:
 *   call 1  → get_deployment_count  (returns registryIds.length as u32)
 *   call 2+ → get_deployments_paginated  (returns address slice)
 *
 * Using call-order routing keeps the mock simple and avoids introspecting
 * the XDR-encoded Transaction object.
 *
 * The response shape must satisfy `StellarSdk.rpc.Api.isSimulationSuccess`,
 * which checks for `"transactionData" in sim`.  We also include `result` so
 * `fetchFactoryRegistry` can read `retval`.
 */
function makeSimulateMock(registryIds: string[]) {
  let callCount = 0;
  return jest.fn().mockImplementation(async () => {
    callCount += 1;

    // Minimal shape that passes isSimulationSuccess ("transactionData" in sim)
    // and carries a result.retval the caller can decode with scValToNative.
    const base = {
      transactionData: {},   // presence is all isSimulationSuccess checks
      minResourceFee: "0",
      events: [],
      _parsed: true,
      id: "mock",
      latestLedger: 100_000,
    };

    if (callCount === 1) {
      // First simulate call is always get_deployment_count.
      return {
        ...base,
        result: {
          auth: [],
          retval: StellarSdk.nativeToScVal(registryIds.length, { type: "u32" }),
        },
      };
    }
    // Subsequent calls are get_deployments_paginated pages.
    // For test purposes we return all ids in one shot (registryIds.length <= 100).
    return {
      ...base,
      result: {
        auth: [],
        retval: StellarSdk.nativeToScVal(registryIds),
      },
    };
  });
}

/** Builds a Server mock; getEvents is a jest.fn() the caller configures per-test. */
function mockServer(options: {
  latestLedger?: number;
  health?: { oldestLedger?: number; ledgerRetentionWindow?: number } | "unsupported";
  getEvents: jest.Mock;
  registryIds?: string[];
}) {
  const { latestLedger = 100_000, health, getEvents, registryIds = [] } = options;

  const getHealth =
    health === "unsupported"
      ? jest.fn().mockRejectedValue(new Error("getHealth not supported"))
      : jest.fn().mockResolvedValue({
          status: "healthy",
          latestLedger,
          ledgerRetentionWindow: health?.ledgerRetentionWindow ?? 120_960,
          oldestLedger: health?.oldestLedger ?? latestLedger - 120_960,
        });

  (StellarSdk.rpc.Server as unknown as jest.Mock).mockImplementation(() => ({
    getLatestLedger: jest.fn().mockResolvedValue({ sequence: latestLedger }),
    getHealth,
    getEvents,
    simulateTransaction: makeSimulateMock(registryIds),
  }));
}

beforeEach(() => {
  jest.clearAllMocks();
  mockFetchTokenInfo.mockImplementation(async (contractId: string) => ({
    contractId,
    name: "Token",
    symbol: "TOK",
  }));
});

describe("fetchRecentTokens — retention window", () => {
  it("uses getHealth().oldestLedger as the lookback floor instead of the fixed 24h window", async () => {
    const getEvents = jest.fn().mockResolvedValue({ events: [] });
    mockServer({
      latestLedger: 200_000,
      health: { oldestLedger: 50_000 },
      getEvents,
    });

    await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    // First call is the init-event scan; startLedger should be the probed
    // oldestLedger (50,000), far wider than the old fixed 17,280-ledger window.
    const firstCallArgs = getEvents.mock.calls[0][0];
    expect(firstCallArgs.startLedger).toBe(50_000);
  });

  it("degrades to the fixed fallback window when getHealth is unsupported", async () => {
    const getEvents = jest.fn().mockResolvedValue({ events: [] });
    mockServer({
      latestLedger: 200_000,
      health: "unsupported",
      getEvents,
    });

    await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    const firstCallArgs = getEvents.mock.calls[0][0];
    // latestLedger (200,000) - FALLBACK_LOOKBACK_LEDGERS (17,280)
    expect(firstCallArgs.startLedger).toBe(200_000 - 17_280);
  });

  it("never requests a startLedger below 1 even if oldestLedger is 0 or missing", async () => {
    const getEvents = jest.fn().mockResolvedValue({ events: [] });
    mockServer({
      latestLedger: 5,
      health: { oldestLedger: 0 },
      getEvents,
    });

    await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    const firstCallArgs = getEvents.mock.calls[0][0];
    expect(firstCallArgs.startLedger).toBeGreaterThanOrEqual(1);
  });
});

describe("fetchRecentTokens — factory registry filter (issue #470)", () => {
  it("returns an empty result when the init event comes from a contract not in the factory registry", async () => {
    // CONTRACT_FOREIGN emits an init event but is NOT in the factory registry,
    // simulating a third-party SEP-41 token or another project's contract.
    const foreignContract = "CFOREIGN_NOT_SOROPAD_TOKEN_AAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const initEvents = [makeInitEvent(foreignContract, 100)];

    const getEvents = jest.fn().mockResolvedValue({ events: initEvents });

    // The factory registry is empty — it knows nothing about this contract.
    mockServer({ getEvents, registryIds: [] });

    const result = await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    expect(result).toEqual([]);
    // fetchTokenInfo must never be called for a non-registry contract.
    expect(mockFetchTokenInfo).not.toHaveBeenCalled();
  });

  it("returns only the subset of event contracts that are in the factory registry", async () => {
    const soropadToken = "CSOROPAD_TOKEN_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const foreignToken = "CFOREIGN_NOT_SOROPAD_TOKEN_AAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    const initEvents = [
      makeInitEvent(soropadToken, 200),
      makeInitEvent(foreignToken, 201), // newer, but not in registry
    ];

    const getEvents = jest
      .fn()
      .mockResolvedValueOnce({ events: initEvents }) // init scan
      .mockResolvedValue({ events: [] }); // scoring batches

    // Only soropadToken is known to the factory.
    mockServer({ getEvents, registryIds: [soropadToken] });

    const result = await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    expect(result).toHaveLength(1);
    expect(result[0].contractId).toBe(soropadToken);
    // The foreign contract must never be hydrated.
    expect(mockFetchTokenInfo).not.toHaveBeenCalledWith(foreignToken, expect.anything());
  });

  it("returns an empty result when no factoryAddress is configured", async () => {
    const initEvents = [makeInitEvent("CSOME_TOKEN_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 100)];
    const getEvents = jest.fn().mockResolvedValue({ events: initEvents });
    mockServer({ getEvents });

    // No factoryAddress passed — the registry cannot be consulted.
    const result = await fetchRecentTokens(mockConfig, "");

    expect(result).toEqual([]);
    expect(mockFetchTokenInfo).not.toHaveBeenCalled();
  });
});

describe("fetchRecentTokens — sort before truncate", () => {
  it("keeps the newest candidates when more than MAX_CANDIDATES(20) launches are found", async () => {
    // 25 candidates returned in an arbitrary (non-ledger-ordered) sequence —
    // simulates RPC event order rather than recency order.
    const totalCandidates = 25;
    const initEvents = Array.from({ length: totalCandidates }, (_, i) => {
      // Deliberately scramble insertion order relative to ledger recency.
      const ledger = ((i * 7) % totalCandidates) + 1;
      return makeInitEvent(`CONTRACT_${i}`, ledger);
    });

    // All 25 contracts are in the factory registry.
    const registryIds = initEvents.map((e) => e.contractId);

    const getEvents = jest
      .fn()
      .mockResolvedValueOnce({ events: initEvents }) // init scan
      .mockResolvedValue({ events: [] }); // scoring batches

    mockServer({ getEvents, registryIds });

    const result = await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    // The 20 candidates actually fetched via fetchTokenInfo should be exactly
    // the 20 with the highest ledger numbers (25 down to 6), not whichever 20
    // happened to appear first in the unsorted event list.
    const fetchedIds = mockFetchTokenInfo.mock.calls.map((c) => c[0] as string);
    expect(fetchedIds).toHaveLength(20);

    const ledgerOf = (id: string) => {
      const evt = initEvents.find((e) => e.contractId === id)!;
      return evt.ledger;
    };
    const fetchedLedgers = fetchedIds.map(ledgerOf).sort((a, b) => b - a);
    const expectedTopLedgers = initEvents
      .map((e) => e.ledger)
      .sort((a, b) => b - a)
      .slice(0, 20);
    expect(fetchedLedgers).toEqual(expectedTopLedgers);
    expect(result).toBeDefined();
  });
});

describe("fetchRecentTokens — concurrent scoring batches", () => {
  it("issues all activity-scoring batches concurrently rather than one at a time", async () => {
    // 12 candidates -> 3 batches of 5/5/2 in the scoring loop.
    const initEvents = Array.from({ length: 12 }, (_, i) =>
      makeInitEvent(`CONTRACT_${i}`, 100 + i),
    );

    // All 12 contracts are registered in the factory.
    const registryIds = initEvents.map((e) => e.contractId);

    let concurrentInFlight = 0;
    let maxConcurrentObserved = 0;

    const getEvents = jest.fn().mockImplementation(async (req: { contractIds?: string[] }) => {
      // The first call (init scan) has a topics filter, not contractIds —
      // only count the scoring-batch calls.
      if (!req.contractIds) {
        return { events: initEvents };
      }
      concurrentInFlight += 1;
      maxConcurrentObserved = Math.max(maxConcurrentObserved, concurrentInFlight);
      await new Promise((resolve) => setTimeout(resolve, 10));
      concurrentInFlight -= 1;
      return { events: [] };
    });

    mockServer({ getEvents, registryIds });

    await fetchRecentTokens(mockConfig, MOCK_FACTORY_ADDRESS);

    // 12 candidates in batches of 5 -> 3 scoring batches (5, 5, 2). If they
    // ran sequentially, maxConcurrentObserved would never exceed 1.
    expect(maxConcurrentObserved).toBeGreaterThan(1);
  });
});
