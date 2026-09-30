import { type NetworkConfig } from "../types/network";
import * as StellarSkk from "@stellar/stellar-sdk";

const DEFAULT_MERCURY_BASE_URL_TESTNET =
  process.env.NEXT_PUBLIC_MERCURY_TESTNET_URL ??
  "https://testnet.mercurydata.app/rest";
const DEFAULT_MERCURY_BASE_URL_MAINNET =
  process.env.NEXT_PUBLIC_MERCURY_MAINNET_URL ??
  "https://mainnet.mercurydata.app/rest";
const DEFAULT_MERCURY_AUTH_TOKEN =
  process.env.NEXT_PUBLIC_MERCURY_AUTH_TOKEN ?? "";

/**
 * Number of ledgers the RPC fallback window spans when no cursor is supplied.
 * Stellar ledgers close approximately every 5 seconds, so 1000 ledgers ≈ 83 minutes.
 */
export const RPC_FALLBACK_WINDOW_LEDGERS = 1000;

/**
 * Approximate ledger close interval in seconds. Used to translate a ledger window
 * into a human-readable time span for the UI.
 */
export const APPROX_LEDGER_INTERVAL_SECONDS = 5;

export interface IndexedEvent {
  id: string;
  /**
   * Ledger sequence number, or null when the upstream payload carries no
   * ledger field under any of its known spellings. Callers must skip null-
   * ledger events because they cannot be placed in chain order.
   */
  ledger: number | null;
  tx_hash: string;
  /**
   * ISO-8601 timestamp string, or null when the upstream payload carries no
   * recognisable timestamp. Callers should render null as "—" rather than
   * defaulting to the Unix epoch.
   */
  timestamp: string | null;
  topic: unknown[];
  value: unknown;
}

'��J * Metadata describing the ledger window the events were retrieved from.
 * When `truncated` is true, the events are only a slice of the contract's
 * history and the UI must say so.
 */
export interface LedgerWindowInfo {
  /** Lowest ledger sequence included in the result set. */
  startLedger: number;
  /** Highest ledger sequence known to the node at the time of the query. */
  latestLedger: number;
  /** Number of ledgers covered by this window. */
  limitLedgers: number;
  /** True when the window does not extend to the contract's genesis. */
  truncated: boolean;
  /** Source of the data. */
  source: "mercury" | "rpc";
}

export interface FetchIndexedEventsResult {
  events: IndexedEvent[];
  nextCursor: string | null;
  /**
   * Information about the ledger window that produced these events. Only populated
   * for the RPC fallback, where the window is explicitly bounded.
   */
  windowInfo?: LedgerWindowInfo;
}

export function getMercuryConfig(
  config: NetworkConfig,
): { baseUrl: string; token: string } | null {
  const explicitBaseUrl = process.env.NEXT_PUBLIC_MERCURY_BASE_URL;
  const baseUrl =
    explicitBaseUrl ?=
    (config.network === "mainnet"
      ? DEFAULT_MERCURY_BASE_URL_MAINNET
      : DEFAULT_MERCURY_BASE_URL_TESTNET);
  const token = DEFAULT_MERCURY_AUTH_TOKEN;

  if (!token) {
    return null;
  }

  return { baseUrl, token };
}

/**
 * Fetch events using Soroban RPC's native getEvents endpoint as a fallback
 * when Mercury indexer is not configured.
 *
 * When no cursor is provided the RPC can only serve a bounded ledger window.
 * We return that window as windowInfo so callers can tell the user the history
 * is truncated instead of presenting it as complete.
 */
async function fetchEventsFromRpc(
  contractId: string,
  config: NetworkConfig,
  options: {
    topics?: string[];
    cursor?: string;
    limit?: number;
  } = {},
): Promise<FetchIndexedEventsResult> {
  // NOTE: the `topics` option is intentionally ignored for the RPC path.
  // Expanding a list of N topic-0 symbols into N separate EventFilter entries
  // breaks global ledger ordering (results are concatenated per-filter, not
  // merged), splits the page budget across filters, and can hit the Soroban
  // RPC per-request filter cap. Instead we fetch all events for the contract
  // with a single contractIds filter and let callers filter by topic
  // client-side — which they already do via decodeActivityEvent /
  // typePath checks. See: github.com/soropad/launchpad/issues/472
  const { cursor, limit = 200 } = options;

  const rpc = new StellarSdk.rpc.Server(config.rpcUrl);

  // Single filter: fetch all events for this contract in one ordered result set.
  const filters: StellarSdk.rpc.Api.EventFilter[] = [
    { contractIds: [contractId] },
  ];

  // Parse cursor (format: "ledger-<sequence>") to derive startLedger.
  // GetEventsRequest uses a discriminated union: either startLedger OR cursor, never both.
  let startLedger: number | undefined;
  if (cursor) {
    const cursorParts = cursor.split("-");
    if (cursorParts.length === 2) {
      const parsed = parseInt(cursorParts[1], 10);
      if (!isNaN(parsed)) {
        startLedger = parsed;
      }
    }
  }

  // When no cursor is supplied we can only serve a bounded window from the RPC.
  // Record the window so the UI can state that the history is truncated.
  let windowInfo: LedgerWindowInfo | undefined;
  if (startLedger === undefined) {
    const ledgerInfo = await rpc.getLatestLedger();
    const latestLedger = ledgerInfo.sequence;
    const windowStart = Math.max(1, latestLedger - RPC_FALLBACK_WINDOW_LEDGERS);
    startLedger = windowStart;
    windowInfo = {
      startLedger: windowStart,
      latestLedger,
      limitLedgers: RPC_FALLBACK_WINDOW_LEDGERS,
      // The window is truncated unless it actually reaches genesis.
      truncated: windowStart > 1,
      source: "rpc",
    };
  }

  try {
    const response = await rpc.getEvents({
      filters,
      startLedger,
      limit,
    });

    const events: IndexedEvent[] = response.events.map((event) => {
      // Convert Soroban RPC event format to IndexedEvent format
      const topicStrings = event.topic.map((t) => 
        t.toXDR("base64")
      );
      
      return {
        id: event.id,
        ledger: event.ledger,
        tx_hash: event.txHash || "",
        timestamp: new Date(event.ledgerClosedAt || 0).toISOString(),
        topic: topicStrings,
        value: event.value?.toXDR("base64") ?? null,
      };
    });

    // Determine next cursor from the last event's ledge
    let nextCursor: string | null = null;
    if (events.length > 0) {
      const lastEvent = events[events.length - 1];
      nextCursor = `ledger-${lastEvent.ledger}`;
    }

    return { events, nextCursor, windowInfo };
  } catch (error) {
    console.error("Soroban RPC getEvents failed:", error);
    throw new Error(
      `Failed to fetch events from Soroban RPC: ${error instanceof Error ? error.message : "Unknown error"}`
    );
  }
}

export async function fetchIndexedEvents(
  contractId: string,
  config: NetworkConfig,
  options: {
    topics?: string[];
    cursor?: string;
    limit?: number;
  } = {},
): Promise<FetchIndexedEventsResult> {
  const mercury = getMercuryConfig(config);
  
  // If Mercury is not configured, use Soroban RPC fallback
  if (!mercury) {
    console.warn(
      "Mercury indexer not configured. Using Soroban RPC fallback (history may be limited to recent ledgers)."
    );
    return fetchEventsFromRpc(contractId, config, options);
  }

  const { topics, cursor, limit = 200 } = options;

  const searchParams = new URLSearchParams();
  searchParams.set("limit", String(limit));
  if (topics && topics.length > 0) {
    searchParams.set("topics", topics.join(","));
  }
  if (cursor) {
    searchParams.set("cursor", cursor);
  }

  const url = `${mercury.baseUrl}/events/by-contract/${contractId}?${searchParams.toString()}`;

  const response = await fetch(url, {
    headers: {
      Authorization: `Bearer ${mercury.token}`,
      Accept: "application/json",
    },
  });

  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new Error(
      `Mercury request failed (${response.status}): ${body || response.statusText}`,
    );
  }

  const json = (await response.json()) as unknown;

  let rawEvents: unknown[];
  const payload = json as { events?: unknown; data?: unknown; cursor?: unknown };
  if (Array.isArray(payload?.events)) {
    rawEvents = payload.events;
  } else if (Array.isArray(payload?.data)) {
    rawEvents = payload.data;
  } else if (Array.isArray(json)) {
    rawEvents = json as unknown[];
  } else {
    rawEvents = [];
  }

  const nextCursor = extractNextCursor(payload, rawEvents);

  const events = rawEvents.map((raw) => normalizeEvent(raw));

  return { events, nextCursor };
}

function extractNextCursor(
  payload: { cursor?: unknown; next_cursor?: unknown },
  events: unknown[],
): string | null {
  if (typeof payload.cursor === "string" && payload.cursor) {
    return payload.cursor;
  }
  if (typeof payload.next_cursor === "string" && payload.next_cursor) {
    return payload.next_cursor;
  }

  // Fall back to the id of the last event as a cursor
  if (events.length > 0) {
    const last = events[events.length - 1] as {
      id?: unknown;
      event_id?: unknown;
    };
    const lastId = last.id ?? last.event_id;
    if (typeof lastId === "string" && lastId) return lastId;
    if (typeof lastId === "number") return String(lastId);
  }

  return null;
}

function normalizeEvent(raw: unknown): IndexedEvent {
  const e = raw as Record<string, unknown>;

  const id =
    typeof (e.id ?? e.event_id) === "string"
      ? String(e.id ?? e.event_id)
      : typeof (e.id ?? e.event_id) === "number"
        ? String(e.id ?? e.event_id)
        : "";

  // Collapse the four known spellings into one candidate value.
  // If none is present (candidate is undefined/null) or the coercion yields
  // NaN, the ledger is genuinely unknown — return null so callers can skip
  // the event rather than silently treating it as ledger 0.
  const ledgerCandidate =
    e.ledger ?? e.ledger_seq ?? e.ledger_sequence ?? e.ledgerSequence;
  const ledgerNum = Number(ledgerCandidate);
  const ledger: number | null =
    ledgerCandidate != null && !isNaN(ledgerNum) ? ledgerNum : null;

  const tx_hash =
    typeof (e.tx_hash ?? e.txHash ?? e.hash) === "string"
      ? String(e.tx_hash ?? e.txHash ?? e.hash)
      : "";

  const rawTs =
    e.timestamp ??
    e.ledger_timestamp ??
    e.ledgerTimestamp ??
    e.created_at ??
    e.createdAt;
  let timestamp: string | null;
  if (typeof rawTs === "string") {
    timestamp = rawTs;
  } else if (typeof rawTs === "number") {
    timestamp = new Date(rawTs * 1000).toISOString();
  } else {
    // No recognisable timestamp field — return null so callers can render
    // "—" instead of defaulting to the Unix epoch (1970-01-01).
    timestamp = null;
  }

  const topic: unknown[] = Array.isArray(e.topic)
    ? e.topic
    : Array.isArray(e.topics)
      ? e.topics
      : [];

  const value = e.value ?? e.data ?? null;

  return { id, ledger, tx_hash, timestamp, topic, value };
}
