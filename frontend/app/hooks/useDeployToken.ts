"use client";

import { useCallback } from "react";
import * as StellarSdk from "@stellar/stellar-sdk";
import { useWallet } from "./useWallet";
import { useNetwork } from "../providers/NetworkProvider";
import { toBaseUnits } from "@/lib/utils";
import { verifyFactory } from "@/lib/stellar";
import { Client as TokenClient } from "@/lib/bindings/token/src/index";
import type { AssembledTransaction } from "@stellar/stellar-sdk/contract";

/**
 * Cryptographically secure random bytes for the deploy salt.
 *
 * The salt determines the token's address, so a predictable salt lets anyone
 * precompute that address and interact with it before the issuer does. There
 * is deliberately no non-cryptographic fallback: if Web Crypto is missing,
 * something is wrong with the environment and the deploy is refused.
 */
function randomBytes(length: number): Buffer {
  const webCrypto = globalThis.crypto;
  if (!webCrypto || typeof webCrypto.getRandomValues !== "function") {
    throw {
      message:
        "Secure random number generation (crypto.getRandomValues) is unavailable in this browser, so a deploy salt cannot be generated safely. Please use an up-to-date browser and try again.",
      type: "validation",
    } as DeployTokenError;
  }
  const array = new Uint8Array(length);
  webCrypto.getRandomValues(array);
  return Buffer.from(array);
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/**
 * When `"true"`, deploy tokens the legacy way: a plain `createContract` deploy
 * followed by a **separate** `initialize` transaction. This is the pre-#368
 * behaviour and exists as a fallback while the factory contract is rolled out.
 *
 * Defaults to `false` — new deployments go through the factory, which deploys
 * and initialises the token in a single atomic transaction (no front-running
 * window between deploy and initialize).
 *
 * Read at call time rather than module scope: Next.js inlines
 * `NEXT_PUBLIC_*` at build time, so this is equivalent for the shipped app,
 * but makes the hook straight-forward to test by swapping `process.env`.
 */
function getDeployConfig() {
  return {
    useLegacyDeploy: process.env.NEXT_PUBLIC_USE_LEGACY_DEPLOY === "true",
    factoryAddress: process.env.NEXT_PUBLIC_FACTORY_ADDRESS ?? "",
    tokenWasmHash: process.env.NEXT_PUBLIC_TOKEN_WASM_HASH ?? "",
  };
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface DeployTokenParams {
  name: string;
  symbol: string;
  decimals: number;
  initialSupply: string;
  maxSupply?: string;
  adminAddress: string;
  authorizationRequired?: boolean;
  authorizationRevocable?: boolean;
  complianceNodeAddress?: string;
  /**
   * Canonical metadata JSON to be keccak256-hashed into the registry in the
   * same transaction as the deploy. `undefined` → `None` in the contract,
   * which leaves `metadata_digest` as `None` in the registry record.
   */
  metadata?: string;
  /**
   * Optional `https://` or `ipfs://` URI forwarded to the token's own
   * `contract_uri` storage so the document location is on-chain from day one.
   */
  contractUri?: string;
  /**
   * Pre-generated deploy salt. When supplied the factory will use this salt
   * to derive the deterministic token address shown in the attestation panel
   * (same bytes, same address). When omitted a fresh random salt is generated
   * at deploy time.
   */
  salt?: Uint8Array;
}

export interface DeployTokenResult {
  contractId: string;
  transactionHash: string;
}

export interface DeployTokenError {
  message: string;
  type: "validation" | "simulation" | "wallet" | "broadcast" | "timeout" | "network";
  /**
   * Set on `timeout` errors when the token address is already known. It comes
   * from simulation, so it is authoritative whether or not the transaction
   * has landed yet, and callers can route to the token's dashboard.
   */
  contractId?: string;
  /** Hash of the broadcast transaction, set on `timeout` errors. */
  transactionHash?: string;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Custom hook for deploying a Soroban SEP-41 token contract.
 *
 * By default the token is deployed through the atomic factory: a single
 * `deploy_token` invocation that both creates the contract and calls
 * `initialize` on it (either happens or neither does). Set
 * `NEXT_PUBLIC_USE_LEGACY_DEPLOY=true` to fall back to the two-transaction
 * legacy path (deploy, then initialize).
 */
export function useDeployToken() {
  const { connected, publicKey, signTransaction } = useWallet();
  const { networkConfig } = useNetwork();

  const deployToken = useCallback(
    async (params: DeployTokenParams): Promise<DeployTokenResult> => {
      if (!connected || !publicKey) {
        throw {
          message: "Wallet not connected. Please connect your wallet and try again.",
          type: "validation",
        } as DeployTokenError;
      }

      const ctx: DeployContext = {
        publicKey,
        signTransaction,
        rpcUrl: networkConfig.rpcUrl,
        passphrase: networkConfig.passphrase,
      };

      const { useLegacyDeploy, factoryAddress, tokenWasmHash } = getDeployConfig();

      if (useLegacyDeploy) {
        if (!tokenWasmHash) {
          throw {
            message:
              "Token WASM hash not configured. Please set NEXT_PUBLIC_TOKEN_WASM_HASH in your environment.",
            type: "validation",
          } as DeployTokenError;
        }
        return deployLegacy(params, ctx, tokenWasmHash);
      }

      if (!factoryAddress) {
        throw {
          message:
            "Factory contract not configured. Please set NEXT_PUBLIC_FACTORY_ADDRESS in your environment.",
          type: "validation",
        } as DeployTokenError;
      }

      const check = await verifyFactory(factoryAddress, networkConfig);
      if (!check.ok) {
        const hashes =
          check.expected || check.actual
            ? ` Expected: ${check.expected ?? "n/a"}. Found: ${check.actual ?? "n/a"}.`
            : "";
        throw {
          message: `Deployment blocked: ${check.reason}${hashes}`,
          type: "validation",
        } as DeployTokenError;
      }
      return deployViaFactory(params, ctx, factoryAddress);
    },
    [connected, publicKey, signTransaction, networkConfig.rpcUrl, networkConfig.passphrase]
  );

  return { deployToken };
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/**
 * Convert a decimal string to base units, translating the `RangeError` thrown
 * by `toBaseUnits` when the input has more decimal places than the token
 * supports into a `DeployTokenError` with `type: "validation"`.
 */
function toBaseUnitsOrThrow(
  value: string,
  decimals: number,
  field: string,
): bigint {
  try {
    return toBaseUnits(value, decimals);
  } catch (err) {
    if (err instanceof RangeError) {
      throw {
        message: `${field}: ${err.message}`,
        type: "validation",
      } as DeployTokenError;
    }
    throw err;
  }
}

interface DeployContext {
  publicKey: string;
  signTransaction: (xdr: string, opts?: { networkPassphrase?: string }) => Promise<string>;
  rpcUrl: string;
  passphrase: string;
}

/**
 * Load the deployer's account, turning "not found" into an actionable
 * validation error. A freshly created or unfunded address has no ledger
 * entry until its first transaction, which is a funding problem rather than
 * a deploy failure.
 */
async function loadSourceAccount(
  rpc: StellarSdk.rpc.Server,
  publicKey: string,
): Promise<StellarSdk.Account> {
  try {
    return await rpc.getAccount(publicKey);
  } catch (err) {
    const status = (err as { response?: { status?: number } } | null)?.response?.status;
    const errorMsg = err instanceof Error ? err.message : String(err);
    // stellar-sdk reports *any* getLedgerEntry failure, transport errors
    // included, as "Account not found", so only blame the account once the
    // RPC has been shown to answer.
    const notFound =
      (status === 404 || /not found/i.test(errorMsg)) &&
      (await rpc.getLatestLedger().then(
        () => true,
        () => false,
      ));
    if (notFound) {
      throw {
        message: `Account ${publicKey} has no transactions on this network yet. Fund it with XLM (on testnet, use Friendbot) and try again.`,
        type: "validation",
      } as DeployTokenError;
    }
    throw {
      message: `Could not load account ${publicKey} from the Soroban RPC: ${errorMsg}`,
      type: "network",
    } as DeployTokenError;
  }
}

const POLL_INTERVAL_MS = 2_000;
/**
 * How long to wait for a broadcast transaction to settle. A mainnet
 * transaction in a busy ledger can legitimately take longer than a minute.
 */
const POLL_BUDGET_MS = 180_000;

/**
 * Broadcast a signed XDR and poll until the transaction settles.
 *
 * `contractId`, when the caller already knows it, is attached to the timeout
 * error so the UI can still send the user to the token rather than asking
 * them to look it up by transaction hash.
 */
async function sendAndPoll(
  signedXdr: string,
  ctx: DeployContext,
  contractId?: string,
): Promise<string> {
  const rpc = new StellarSdk.rpc.Server(ctx.rpcUrl);
  const signedTx = StellarSdk.TransactionBuilder.fromXDR(
    signedXdr,
    ctx.passphrase,
  ) as StellarSdk.Transaction;

  let sendResult: StellarSdk.rpc.Api.SendTransactionResponse;
  try {
    sendResult = await rpc.sendTransaction(signedTx);
  } catch (err) {
    throw {
      message: `Broadcast failed: ${err instanceof Error ? err.message : String(err)}`,
      type: "broadcast",
    } as DeployTokenError;
  }

  if (sendResult.status === "ERROR") {
    throw {
      message: `Transaction submission failed: ${sendResult.errorResult?.toXDR("base64") || "Unknown error"}`,
      type: "broadcast",
    } as DeployTokenError;
  }

  const txHash = sendResult.hash;
  const maxAttempts = Math.ceil(POLL_BUDGET_MS / POLL_INTERVAL_MS);
  // Transport error from the most recent poll, if it failed. Lets a timeout
  // say "the RPC is unreachable" instead of "the transaction has not settled".
  let lastPollError: unknown = null;

  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL_MS));

    let getResult: StellarSdk.rpc.Api.GetTransactionResponse;
    try {
      getResult = await rpc.getTransaction(txHash);
    } catch (err) {
      lastPollError = err;
      continue;
    }
    lastPollError = null;

    if (getResult.status === "SUCCESS") {
      return txHash;
    }
    if (getResult.status === "FAILED") {
      throw {
        message: `Transaction failed: ${getResult.resultXdr?.toXDR("base64") || "Unknown failure"}`,
        type: "broadcast",
      } as DeployTokenError;
    }
  }

  const tokenNote = contractId ? ` The token address is ${contractId}.` : "";
  const message = lastPollError
    ? `Could not reach the Soroban RPC to confirm transaction ${txHash} (${
        lastPollError instanceof Error ? lastPollError.message : String(lastPollError)
      }). The transaction may still have succeeded.${tokenNote}`
    : `Transaction ${txHash} has not settled after ${
        POLL_BUDGET_MS / 1000
      } seconds. It may still be included in a later ledger.${tokenNote}`;

  throw {
    message,
    type: "timeout",
    contractId,
    transactionHash: txHash,
  } as DeployTokenError;
}

/** Ask the wallet to sign a prepared transaction, normalising user rejection. */
async function signPrepared(prepared: StellarSdk.Transaction, ctx: DeployContext): Promise<string> {
  try {
    return await ctx.signTransaction(prepared.toXDR(), {
      networkPassphrase: ctx.passphrase,
    });
  } catch (err) {
    const errorMsg = err instanceof Error ? err.message : String(err);
    if (
      errorMsg.toLowerCase().includes("user declined") ||
      errorMsg.toLowerCase().includes("user rejected") ||
      errorMsg.toLowerCase().includes("cancelled")
    ) {
      throw {
        message: "Transaction signature was rejected. Please try again.",
        type: "wallet",
      } as DeployTokenError;
    }
    throw {
      message: `Wallet signing failed: ${errorMsg}`,
      type: "wallet",
    } as DeployTokenError;
  }
}

/**
 * Build the `TokenConfig` struct ScVal for the factory's `deploy_token` call.
 * The struct is encoded as a map with symbol keys; `admin` / `compliance_node`
 * are Addresses, numeric fields get explicit widths, and optional fields pass
 * `null` for `None`.
 */
function buildTokenConfigScVals(
  params: DeployTokenParams,
  deployer: string,
): StellarSdk.xdr.ScVal {
  const maxSupply = params.maxSupply
    ? toBaseUnitsOrThrow(params.maxSupply, params.decimals, "maxSupply")
    : null;
  const complianceNode =
    params.complianceNodeAddress && params.complianceNodeAddress.trim().length > 0
      ? params.complianceNodeAddress.trim()
      : null;

  return StellarSdk.nativeToScVal(
    {
      admin: params.adminAddress || deployer,
      authorization_required: params.authorizationRequired ?? false,
      authorization_revocable: params.authorizationRevocable ?? false,
      compliance_node: complianceNode,
      contract_uri: params.contractUri ?? null,
      decimal: params.decimals,
      initial_supply: toBaseUnitsOrThrow(params.initialSupply, params.decimals, "initialSupply"),
      max_supply: maxSupply,
      metadata: params.metadata ?? null,
      name: params.name,
      symbol: params.symbol,
    },
    {
      type: {
        admin: ["symbol", "address"],
        compliance_node: ["symbol", "address"],
        contract_uri: ["symbol", "string"],
        decimal: ["symbol", "u32"],
        initial_supply: ["symbol", "i128"],
        max_supply: ["symbol", "i128"],
        metadata: ["symbol", "string"],
      },
    },
  );
}

/**
 * Extract the contract address returned by a `createHostFunction` /
 * `createCustomContract` operation from a successful transaction's soroban
 * return value.
 */
function extractContractId(
  result: StellarSdk.rpc.Api.GetTransactionResponse,
): string | null {
  if (result.status !== "SUCCESS" || !result.resultMetaXdr) {
    return null;
  }
  try {
    const returnValue = result.resultMetaXdr.v3()?.sorobanMeta()?.returnValue();
    if (!returnValue) return null;
    return StellarSdk.Address.fromScVal(returnValue).toString();
  } catch (err) {
    console.error("Failed to extract contract ID:", err);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Factory path
// ---------------------------------------------------------------------------

/**
 * Deploy + initialise a token in a single atomic transaction via the factory.
 *
 * `deploy_token(deployer, salt, config)` returns the deterministic token
 * address. The factory enforces `deployer.require_auth()` and the token's own
 * `initialize` enforces `admin.require_auth()`. The frontend passes the
 * deployer's own public key as `admin`, so a single wallet signature covers
 * both authorisations. The call is forwarded here as a raw contract invocation
 * (the factory WASM embeds the token spec, which trips up the TS binding
 * generator, so we hand-roll it).
 */
async function deployViaFactory(
  params: DeployTokenParams,
  ctx: DeployContext,
  factoryAddress: string,
): Promise<DeployTokenResult> {
  const rpc = new StellarSdk.rpc.Server(ctx.rpcUrl);
  const contract = new StellarSdk.Contract(factoryAddress);
  // Use the caller-supplied salt (shown in the attestation panel) so the
  // on-chain address matches the one the user saw before signing. Fall back
  // to a fresh random salt when none was provided (e.g. legacy callers).
  const salt = params.salt ? Buffer.from(params.salt) : randomBytes(32);

  const account = await loadSourceAccount(rpc, ctx.publicKey);

  const tx = new StellarSdk.TransactionBuilder(account, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: ctx.passphrase,
  })
    .addOperation(
      contract.call(
        "deploy_token",
        new StellarSdk.Address(ctx.publicKey).toScVal(),
        StellarSdk.nativeToScVal(salt),
        buildTokenConfigScVals(params, ctx.publicKey),
      ),
    )
    .setTimeout(300)
    .build();

  const sim = await rpc.simulateTransaction(tx);

  if (StellarSdk.rpc.Api.isSimulationError(sim)) {
    throw {
      message: `Deployment simulation failed: ${sim.error}`,
      type: "simulation",
    } as DeployTokenError;
  }
  if (!StellarSdk.rpc.Api.isSimulationSuccess(sim) || !sim.result) {
    throw {
      message: "Deployment simulation did not produce a result. Please check your parameters and try again.",
      type: "simulation",
    } as DeployTokenError;
  }

  // The factory returns the deterministic token address.
  const contractId = StellarSdk.Address.fromScVal(sim.result.retval).toString();

  const assembled = StellarSdk.rpc.assembleTransaction(tx, sim).build();
  const signedXdr = await signPrepared(assembled, ctx);
  const transactionHash = await sendAndPoll(signedXdr, ctx, contractId);

  return { contractId, transactionHash };
}

// ---------------------------------------------------------------------------
// Legacy path (two-transaction: deploy, then initialize)
// ---------------------------------------------------------------------------

/**
 * Deploy a token the original way: `createContract` (using a pre-uploaded WASM
 * hash) followed by a separate `initialize` transaction. Kept behind
 * `NEXT_PUBLIC_USE_LEGACY_DEPLOY=true` as a fallback while the factory is
 * rolled out.
 */
async function deployLegacy(
  params: DeployTokenParams,
  ctx: DeployContext,
  tokenWasmHash: string,
): Promise<DeployTokenResult> {
  const rpc = new StellarSdk.rpc.Server(ctx.rpcUrl);

  // ── Step 1: Deploy the raw contract ─────────────────────────────────
  const salt = randomBytes(32);
  const sourceAccount = await loadSourceAccount(rpc, ctx.publicKey);
  const wasmHashBuffer = Buffer.from(tokenWasmHash, "hex");

  const deployOp = StellarSdk.Operation.createCustomContract({
    address: new StellarSdk.Address(ctx.publicKey),
    wasmHash: wasmHashBuffer,
    salt,
  });

  const deployTx = new StellarSdk.TransactionBuilder(sourceAccount, {
    fee: StellarSdk.BASE_FEE,
    networkPassphrase: ctx.passphrase,
  })
    .addOperation(deployOp)
    .setTimeout(30)
    .build();

  let simResult: StellarSdk.rpc.Api.SimulateTransactionResponse;
  try {
    simResult = await rpc.simulateTransaction(deployTx);
  } catch (err) {
    throw {
      message: `Simulation request failed: ${err instanceof Error ? err.message : String(err)}`,
      type: "simulation",
    } as DeployTokenError;
  }

  if (StellarSdk.rpc.Api.isSimulationError(simResult)) {
    throw {
      message: `Simulation failed: ${simResult.error}`,
      type: "simulation",
    } as DeployTokenError;
  }
  if (!StellarSdk.rpc.Api.isSimulationSuccess(simResult)) {
    throw {
      message: "Simulation did not succeed. Please check your parameters and try again.",
      type: "simulation",
    } as DeployTokenError;
  }

  const assembledDeployTx = StellarSdk.rpc.assembleTransaction(deployTx, simResult).build();
  const signedDeployXdr = await signPrepared(assembledDeployTx, ctx);
  const deployHash = await sendAndPoll(signedDeployXdr, ctx);

  // Resolve the deterministic contract address from the deploy result meta.
  const deployment = await rpc.getTransaction(deployHash);
  const contractId = extractContractId(deployment);
  if (!contractId) {
    throw {
      message: "Contract deployed but its address could not be extracted from the result.",
      type: "broadcast",
    } as DeployTokenError;
  }

  // ── Step 2: Initialize the token ────────────────────────────────────
  const tokenClient = new TokenClient({
    networkPassphrase: ctx.passphrase,
    contractId,
    rpcUrl: ctx.rpcUrl,
    publicKey: ctx.publicKey,
  });

  let initTx: AssembledTransaction<null>;
  try {
    initTx = await tokenClient.initialize({
      admin: params.adminAddress || ctx.publicKey,
      decimal: params.decimals,
      name: params.name,
      symbol: params.symbol,
      initial_supply: toBaseUnitsOrThrow(params.initialSupply, params.decimals, "initialSupply"),
      max_supply: params.maxSupply
        ? toBaseUnitsOrThrow(params.maxSupply, params.decimals, "maxSupply")
        : undefined,
      authorization_required: params.authorizationRequired ?? false,
      authorization_revocable: params.authorizationRevocable ?? false,
      compliance_node:
        params.complianceNodeAddress && params.complianceNodeAddress.trim().length > 0
          ? params.complianceNodeAddress.trim()
          : undefined,
    });
  } catch (err) {
    throw {
      message: `Initialization simulation failed: ${err instanceof Error ? err.message : String(err)}`,
      type: "simulation",
    } as DeployTokenError;
  }

  if (!initTx.built) {
    throw {
      message: "Initialization simulation did not produce a transaction.",
      type: "simulation",
    } as DeployTokenError;
  }

  const signedInitXdr = await signPrepared(initTx.built, ctx);
  const initHash = await sendAndPoll(signedInitXdr, ctx, contractId);

  return { contractId, transactionHash: initHash };
}
