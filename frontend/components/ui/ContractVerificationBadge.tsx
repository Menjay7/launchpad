"use client";

import { useEffect, useState, useRef } from "react";
import { ShieldCheck, ShieldAlert, ShieldQuestion, Lock, Loader2 } from "lucide-react";
import type { NetworkConfig } from "@/types/network";
import { getContractWasmHash, fetchWasmManifest, simulateCall, decodeString } from "@/lib/stellar";
import * as StellarSdk from "@stellar/stellar-sdk";

type VerificationStatus = "loading" | "verified" | "modified" | "unknown" | "unchecked";

interface ContractVerificationBadgeProps {
  contractId: string;
  networkConfig: NetworkConfig;
  isLocked?: boolean;
  compact?: boolean;
  /**
   * - `"token"` — compares the contract's WASM hash against the manifest's token entry.
   * - `"factory"` — compares against the manifest's per-network factory entry.
   * - `"token-wasm"` — reads `get_token_wasm_hash` from a factory contract and
   *   compares it against the manifest's token WASM entry. `contractId` must be
   *   the factory address.
   */
  kind?: "token" | "factory" | "token-wasm";
}

function shortenHash(hash: string): string {
  return `${hash.slice(0, 8)}...${hash.slice(-6)}`;
}

export function ContractVerificationBadge({
  contractId,
  networkConfig,
  isLocked,
  compact,
  kind = "token",
}: ContractVerificationBadgeProps) {
  const [status, setStatus] = useState<VerificationStatus>("loading");
  const [deployedHash, setDeployedHash] = useState<string | null>(null);
  const [referenceHash, setReferenceHash] = useState<string | null>(null);
  const [referenceVersion, setReferenceVersion] = useState<string | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    let cancelled = false;

    async function verify() {
      setStatus("loading");
      try {
        const [deployed, mfst] = await Promise.all([
          getContractWasmHash(contractId, networkConfig),
          fetchWasmManifest(),
        ]);

        if (cancelled) return;

        if (!deployed) {
          if (mountedRef.current) setStatus("unknown");
          return;
        }

        if (mountedRef.current) setDeployedHash(deployed);

        if (!mfst) {
          if (mountedRef.current) setStatus("unknown");
          return;
        }

        let referenceWasmHash: string | undefined;
        let latestVersion: string | undefined;
        if (kind === "token-wasm") {
          // Read what WASM hash the factory currently points at for token deploys.
          let onChainTokenHash: string | null = null;
          try {
            const retval = await simulateCall(contractId, "get_token_wasm_hash", networkConfig);
            const rawBytes = retval.bytes() as Buffer;
            onChainTokenHash = Buffer.from(rawBytes).toString("hex");
          } catch {
            // factory may not be deployed or RPC unreachable
          }

          if (!onChainTokenHash) {
            if (mountedRef.current) setStatus("unknown");
            return;
          }
          if (mountedRef.current) setDeployedHash(onChainTokenHash);

          const tokenEntry = mfst.token;
          latestVersion = tokenEntry?.latest;
          referenceWasmHash = latestVersion
            ? tokenEntry.versions[latestVersion]?.wasm_hash
            : undefined;

          if (!referenceWasmHash || !latestVersion) {
            if (mountedRef.current) setStatus("unknown");
            return;
          }

          if (mountedRef.current) {
            setReferenceHash(referenceWasmHash);
            setReferenceVersion(latestVersion);
          }

          if (onChainTokenHash === referenceWasmHash) {
            if (mountedRef.current) setStatus("verified");
          } else {
            if (mountedRef.current) setStatus("modified");
          }
          return;
        } else if (kind === "factory") {
          const entry = mfst.factory?.deployments?.[networkConfig.network];
          if (entry?.address && entry.address !== contractId) {
            if (mountedRef.current) setStatus("modified");
            return;
          }
          referenceWasmHash = entry?.wasm_hash;
          latestVersion = "factory";
        } else {
          const tokenEntry = mfst.token;
          latestVersion = tokenEntry?.latest;
          referenceWasmHash = latestVersion
            ? tokenEntry.versions[latestVersion]?.wasm_hash
            : undefined;
        }

        if (!referenceWasmHash || !latestVersion) {
          if (mountedRef.current) setStatus("unknown");
          return;
        }
        const versionData = { wasm_hash: referenceWasmHash };

        if (mountedRef.current) {
          setReferenceHash(versionData.wasm_hash);
          setReferenceVersion(latestVersion);
        }

        if (deployed === versionData.wasm_hash) {
          if (mountedRef.current) setStatus("verified");
        } else {
          if (mountedRef.current) setStatus("modified");
        }
      } catch {
        if (!cancelled && mountedRef.current) setStatus("unknown");
      }
    }

    verify();

    return () => {
      cancelled = true;
      mountedRef.current = false;
    };
  }, [contractId, networkConfig, kind]);

  if (status === "loading") {
    if (compact) return null;
    return (
      <div className="inline-flex items-center gap-1.5 rounded-full bg-gray-500/10 px-2.5 py-0.5 text-xs text-gray-400">
        <Loader2 className="h-3 w-3 animate-spin" />
        Verifying...
      </div>
    );
  }

  if (status === "unknown") {
    if (compact) return null;
    return (
      <div className="inline-flex items-center gap-1.5 rounded-full bg-gray-500/10 px-2.5 py-0.5 text-xs text-gray-400">
        <ShieldQuestion className="h-3 w-3" />
        Unverified
      </div>
    );
  }

  if (status === "verified") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-green-500/10 px-2 py-0.5 text-xs font-medium text-green-400">
        <ShieldCheck className="h-3 w-3" />
        {compact
          ? "Verified"
          : kind === "factory"
            ? "Verified factory"
            : kind === "token-wasm"
              ? `Verified token WASM (v${referenceVersion})`
              : `Verified (v${referenceVersion})`}
        {isLocked && (
          <Lock className="ml-0.5 h-2.5 w-2.5 text-green-300/70" />
        )}
      </span>
    );
  }

  if (status === "modified" && !compact) {
    return (
      <div className="group relative">
        <span className="inline-flex items-center gap-1 rounded-full bg-red-500/10 px-2.5 py-0.5 text-xs font-medium text-red-400">
          <ShieldAlert className="h-3 w-3" />
          Modified
        </span>
        <div className="pointer-events-none absolute bottom-full left-1/2 z-10 mb-2 w-64 -translate-x-1/2 rounded-lg border border-red-500/30 bg-void-900/95 p-3 text-xs opacity-0 shadow-lg transition-opacity group-hover:pointer-events-auto group-hover:opacity-100">
          <p className="mb-1 font-medium text-red-400">Modified Contract</p>
          <p className="mb-1.5 text-gray-400">
            This contract&apos;s WASM hash does not match the reference build. It
            may have been upgraded or deployed from different source code.
          </p>
          <div className="space-y-0.5 font-mono text-[10px] text-gray-500">
            {deployedHash && (
              <p>Deployed: {shortenHash(deployedHash)}</p>
            )}
            {referenceHash && (
              <p>Reference: {shortenHash(referenceHash)}</p>
            )}
          </div>
        </div>
      </div>
    );
  }

  if (status === "modified" && compact) {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-red-500/10 px-2 py-0.5 text-xs font-medium text-red-400">
        <ShieldAlert className="h-3 w-3" />
        Modified
      </span>
    );
  }

  return null;
}
