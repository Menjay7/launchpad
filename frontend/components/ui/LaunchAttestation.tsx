"use client";

import { useEffect, useRef, useState } from "react";
import {
  ShieldCheck,
  ShieldAlert,
  ShieldQuestion,
  Loader2,
  ChevronDown,
  ChevronUp,
} from "lucide-react";
import type { NetworkConfig } from "@/types/network";
import type { LaunchAttestation } from "@/lib/stellar";
import { resolveLaunchAttestation } from "@/lib/stellar";

interface LaunchAttestationPanelProps {
  factoryAddress: string;
  deployer: string;
  /** 32-byte random salt generated for this deploy attempt. */
  salt: Uint8Array;
  networkConfig: NetworkConfig;
  /**
   * Called once resolution completes so the parent can gate the Deploy button.
   * Receives `false` when manifestMatch === "mismatch".
   */
  onResult?: (ok: boolean) => void;
}

function HashRow({ label, value }: { label: string; value: string | null }) {
  if (!value) return null;
  return (
    <div className="flex flex-col gap-0.5 py-2 border-b border-white/5 last:border-0">
      <span className="text-[10px] font-medium uppercase tracking-wider text-gray-500">
        {label}
      </span>
      <span className="font-mono text-[11px] break-all text-gray-300 select-all">
        {value}
      </span>
    </div>
  );
}

/**
 * Displays the four attestation facts (factory hash, token WASM hash,
 * manifest match status, and deterministic token address) before the user
 * signs a deploy transaction.
 *
 * Fail-closed: if `manifestMatch === "mismatch"` the panel renders a hard
 * warning and calls `onResult(false)`, which the parent uses to keep the
 * Deploy button disabled.
 */
export function LaunchAttestationPanel({
  factoryAddress,
  deployer,
  salt,
  networkConfig,
  onResult,
}: LaunchAttestationPanelProps) {
  const [attestation, setAttestation] = useState<LaunchAttestation | null>(
    null,
  );
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState(false);
  const onResultRef = useRef(onResult);
  onResultRef.current = onResult;

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setAttestation(null);

    resolveLaunchAttestation(factoryAddress, deployer, salt, networkConfig)
      .then((result) => {
        if (cancelled) return;
        setAttestation(result);
        setLoading(false);
        onResultRef.current?.(result.manifestMatch !== "mismatch");
      })
      .catch(() => {
        if (cancelled) return;
        setLoading(false);
        onResultRef.current?.(false);
      });

    return () => {
      cancelled = true;
    };
    // salt identity changes every render if passed inline — stabilise with
    // a join so the effect only re-runs when the bytes actually change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [factoryAddress, deployer, salt.join(","), networkConfig]);

  // ── Loading ─────────────────────────────────────────────────────────
  if (loading) {
    return (
      <div className="flex items-center gap-2 rounded-xl border border-white/5 bg-void-800/40 px-4 py-3 text-sm text-gray-400">
        <Loader2 className="h-4 w-4 animate-spin shrink-0 text-stellar-400" />
        Verifying factory and token WASM…
      </div>
    );
  }

  if (!attestation) {
    return (
      <div className="flex items-center gap-2 rounded-xl border border-red-500/20 bg-red-500/5 px-4 py-3 text-sm text-red-400">
        <ShieldAlert className="h-4 w-4 shrink-0" />
        Attestation failed — could not read on-chain hashes.
      </div>
    );
  }

  const { manifestMatch, factoryHash, tokenWasmHash, address, warnings } =
    attestation;

  // ── Mismatch (fail-closed) ────────────────────────────────────────────
  if (manifestMatch === "mismatch") {
    return (
      <div
        className="rounded-xl border border-red-500/30 bg-red-500/5 p-4 space-y-3"
        role="alert"
        aria-live="assertive"
      >
        <div className="flex items-start gap-3">
          <ShieldAlert className="mt-0.5 h-5 w-5 shrink-0 text-red-400" />
          <div>
            <p className="text-sm font-semibold text-red-300">
              WASM hash mismatch — deployment blocked
            </p>
            <p className="mt-1 text-xs text-red-400/80">
              The on-chain code does not match the audited build. Do not sign
              until this is resolved.
            </p>
          </div>
        </div>
        {warnings.length > 0 && (
          <ul className="space-y-1 pl-8">
            {warnings.map((w, i) => (
              <li key={i} className="text-xs text-red-400">
                {w}
              </li>
            ))}
          </ul>
        )}
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="flex items-center gap-1 pl-8 text-xs text-red-400/70 hover:text-red-300 transition-colors"
          aria-expanded={expanded}
        >
          {expanded ? (
            <ChevronUp className="h-3 w-3" />
          ) : (
            <ChevronDown className="h-3 w-3" />
          )}
          {expanded ? "Hide details" : "Show details"}
        </button>
        {expanded && (
          <div className="pl-8 space-y-0">
            <HashRow label="Factory WASM hash" value={factoryHash} />
            <HashRow label="Token WASM hash" value={tokenWasmHash} />
            <HashRow label="Deterministic token address" value={address} />
          </div>
        )}
      </div>
    );
  }

  // ── Unknown (inconclusive — allow but warn) ───────────────────────────
  if (manifestMatch === "unknown") {
    return (
      <div className="rounded-xl border border-yellow-500/20 bg-yellow-500/5 p-4 space-y-2">
        <div className="flex items-start gap-3">
          <ShieldQuestion className="mt-0.5 h-5 w-5 shrink-0 text-yellow-400" />
          <div>
            <p className="text-sm font-medium text-yellow-200">
              Verification inconclusive
            </p>
            <p className="mt-0.5 text-xs text-yellow-400/80">
              {warnings[0] ??
                "The manifest has no reference hashes for this network."}
            </p>
          </div>
        </div>
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="flex items-center gap-1 pl-8 text-xs text-yellow-400/60 hover:text-yellow-300 transition-colors"
          aria-expanded={expanded}
        >
          {expanded ? (
            <ChevronUp className="h-3 w-3" />
          ) : (
            <ChevronDown className="h-3 w-3" />
          )}
          {expanded ? "Hide hashes" : "Show hashes"}
        </button>
        {expanded && (
          <div className="pl-8 space-y-0">
            <HashRow label="Factory WASM hash" value={factoryHash} />
            <HashRow label="Token WASM hash" value={tokenWasmHash} />
            <HashRow label="Deterministic token address" value={address} />
          </div>
        )}
      </div>
    );
  }

  // ── Match (verified) ─────────────────────────────────────────────────
  return (
    <div className="rounded-xl border border-green-500/20 bg-green-500/5 p-4 space-y-2">
      <div className="flex items-start gap-3">
        <ShieldCheck className="mt-0.5 h-5 w-5 shrink-0 text-green-400" />
        <div>
          <p className="text-sm font-semibold text-green-300">
            Audited build verified
          </p>
          <p className="mt-0.5 text-xs text-green-400/70">
            Factory and token WASM hashes match the audited manifest. Your
            wallet&apos;s signature authorises this exact deployment — the
            launchpad cannot substitute different code.
          </p>
        </div>
      </div>
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="flex items-center gap-1 pl-8 text-xs text-green-400/60 hover:text-green-300 transition-colors"
        aria-expanded={expanded}
      >
        {expanded ? (
          <ChevronUp className="h-3 w-3" />
        ) : (
          <ChevronDown className="h-3 w-3" />
        )}
        {expanded ? "Hide hashes" : "Show hashes"}
      </button>
      {expanded && (
        <div className="pl-8 space-y-0">
          <HashRow label="Factory WASM hash" value={factoryHash} />
          <HashRow label="Token WASM hash" value={tokenWasmHash} />
          <HashRow label="Deterministic token address" value={address} />
        </div>
      )}
    </div>
  );
}
