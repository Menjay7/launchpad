"use client";

import { useEffect, useState, useCallback, useRef } from "react";
import { useTranslations } from "next-intl";
import {
  AlertCircle,
  Flame,
  CheckCircle2,
  Plus,
  Trash2,
  ExternalLink,
} from "lucide-react";
import Link from "next/link";
import { 
  fetchWalletDeployments,
  trackDeployment, 
  removeTrackedDeployment,
  type TrackedDeployment 
} from "@/lib/deployments";
import { useWallet } from "../hooks/useWallet";
import { useSoroban } from "@/hooks/useSoroban";
import {
  type AccountBalance,
  type VestingScheduleInfo,
  type TokenActivityInfo,
  truncateAddress,
} from "@/lib/stellar";
import { ConnectPrompt } from "./components/ConnectPrompt";
import { PersonalHeader } from "./components/PersonalHeader";
import { BalancesSection } from "./components/BalancesSection";
import { VestingSection } from "./components/VestingSection";
import { PersonalTransactionsSection } from "./components/PersonalTransactionsSection";
import { OutgoingAllowancesSection } from "./components/OutgoingAllowancesSection";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import { EmptyState } from "@/components/ui/EmptyState";

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export default function PersonalDashboard() {
  const t = useTranslations("myAccount");
  const { connected, publicKey, connect, signTransaction } = useWallet();
  const {
    fetchVestingSchedule,
    fetchAllVestingSchedules,
    fetchCurrentLedger,
    fetchAccountBalances,
    fetchAccountOperations,
    buildBurnTransaction,
    submitTransaction,
    fetchTokenInfo,
    networkConfig,
  } = useSoroban();

  // Balances
  const [balances, setBalances] = useState<AccountBalance[]>([]);
  const [balancesLoading, setBalancesLoading] = useState(false);
  const [balancesError, setBalancesError] = useState<string | null>(null);

  // Vesting
  const [vestingContractId, setVestingContractId] = useState("");
  const [vestingSchedules, setVestingSchedules] = useState<VestingScheduleInfo[]>([]);
  const [vestingLoading, setVestingLoading] = useState(false);
  const [vestingError, setVestingError] = useState<string | null>(null);
  const [currentLedger, setCurrentLedger] = useState(0);

  // Transaction history
  const [operations, setOperations] = useState<TokenActivityInfo[]>([]);
  const [txLoading, setTxLoading] = useState(false);
  const [txLoadingMore, setTxLoadingMore] = useState(false);
  const [txNextCursor, setTxNextCursor] = useState<string | null>(null);
  const cursorRef = useRef<string | null>(null);
  cursorRef.current = txNextCursor;

  // Refreshing state
  const [refreshing, setRefreshing] = useState(false);

  // Burn state
  const [burnContractId, setBurnContractId] = useState("");
  const [burnAmount, setBurnAmount] = useState("");
  const [burnLoading, setBurnLoading] = useState(false);
  const [burnSuccess, setBurnSuccess] = useState(false);
  const [burnError, setBurnError] = useState<string | null>(null);

  // Tracked deployments
  const [trackedTokens, setTrackedTokens] = useState<TrackedDeployment[]>([]);
  const [importContractId, setImportContractId] = useState("");
  const [importLoading, setImportLoading] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);

  const loadTrackedTokens = useCallback(async () => {
    if (publicKey) {
      const factoryAddress = process.env.NEXT_PUBLIC_FACTORY_ADDRESS ?? "";
      const deployments = await fetchWalletDeployments(publicKey, networkConfig, factoryAddress);
      setTrackedTokens(deployments);
    }
  }, [publicKey, networkConfig]);

  const handleImportToken = async () => {
    if (!publicKey || !importContractId.trim()) return;
    setImportLoading(true);
    setImportError(null);
    try {
      const info = await fetchTokenInfo(importContractId.trim());
      // Import is now just for tokens not deployed through the factory
      // or for manual tracking. The main list comes from on-chain data.
      trackDeployment(publicKey, {
        contractId: importContractId.trim(),
        name: info.name,
        symbol: info.symbol,
        network: networkConfig.network,
      });
      setImportContractId("");
      await loadTrackedTokens();
    } catch {
      setImportError(t("importError"));
    } finally {
      setImportLoading(false);
    }
  };

  const handleRemoveToken = (contractId: string) => {
    if (publicKey) {
      removeTrackedDeployment(publicKey, contractId);
      loadTrackedTokens();
    }
  };

  const handleBurn = async () => {
    if (!publicKey || !burnContractId || !burnAmount) return;
    setBurnLoading(true);
    setBurnError(null);
    setBurnSuccess(false);
    try {
      // For simplicity, we assume 7 decimals for now, as is standard for our tokens
      // Improving this would involve fetching decimals first
      const xdr = await buildBurnTransaction(burnContractId, publicKey, burnAmount, 7);
      const signed = await signTransaction(xdr);

      // Submit via Soroban hook
      await submitTransaction(signed);

      setBurnSuccess(true);
      setBurnAmount("");
      // Refresh balances to show the change
      await loadBalances();
    } catch (err) {
      console.error("Burn failed:", err);
      setBurnError(err instanceof Error ? err.message : "Burn transaction failed");
    } finally {
      setBurnLoading(false);
    }
  };

  // Fetch balances
  const loadBalances = useCallback(async () => {
    if (!publicKey) return;
    setBalancesLoading(true);
    setBalancesError(null);
    try {
      const data = await fetchAccountBalances(publicKey);
      setBalances(data);
    } catch (err) {
      setBalancesError(
        err instanceof Error ? err.message : "Failed to fetch balances.",
      );
    } finally {
      setBalancesLoading(false);
    }
  }, [publicKey, fetchAccountBalances]);

  // Fetch personal transactions
  const loadTransactions = useCallback(
    async (isLoadMore = false) => {
      if (!publicKey) return;
      if (!isLoadMore) setTxLoading(true);
      if (isLoadMore) setTxLoadingMore(true);

      try {
        const cursor = isLoadMore ? cursorRef.current : undefined;
        const { records, nextCursor } = await fetchAccountOperations(
          publicKey,
          cursor ?? undefined,
          15,
        );

        if (isLoadMore) {
          setOperations((prev) => [...prev, ...records]);
        } else {
          setOperations(records);
        }
        setTxNextCursor(nextCursor);
      } catch {
        // Silently fail for tx feed on refresh
        if (!isLoadMore) setOperations([]);
      } finally {
        if (!isLoadMore) setTxLoading(false);
        if (isLoadMore) setTxLoadingMore(false);
      }
    },
    [publicKey, fetchAccountOperations],
  );

  // Load all vesting schedules (uses get_all_schedules — single call, not N+1)
  const doVestingLookup = useCallback(async (contractId: string) => {
    if (!publicKey || !contractId.trim()) return;
    setVestingContractId(contractId);
    setVestingLoading(true);
    setVestingError(null);
    setVestingSchedules([]);
    try {
      const [allSchedules, ledger] = await Promise.all([
        fetchAllVestingSchedules(contractId.trim(), publicKey),
        fetchCurrentLedger(),
      ]);

      if (allSchedules.length === 0) {
        setVestingError("No vesting schedule found for your wallet on this contract.");
        setVestingLoading(false);
        return;
      }

      setVestingSchedules(allSchedules);
      setCurrentLedger(ledger);
    } catch (err) {
      setVestingError(
        err instanceof Error
          ? err.message
          : "Failed to fetch vesting schedules. Check the contract ID.",
      );
    } finally {
      setVestingLoading(false);
    }
  }, [publicKey, fetchAllVestingSchedules, fetchCurrentLedger]);

  const lookupVesting = useCallback(async () => {
    await doVestingLookup(vestingContractId);
  }, [vestingContractId, doVestingLookup]);

  // Initial data load
  useEffect(() => {
    if (connected && publicKey) {
      loadBalances();
      loadTransactions();
      loadTrackedTokens();
    }
  }, [connected, publicKey, loadBalances, loadTransactions, loadTrackedTokens]);

  // Refresh all
  const handleRefresh = useCallback(async () => {
    setRefreshing(true);
    await Promise.all([loadBalances(), loadTransactions()]);
    setRefreshing(false);
  }, [loadBalances, loadTransactions]);

  // Not connected
  if (!connected || !publicKey) {
    return <ConnectPrompt onConnect={connect} />;
  }

  return (
    <div className="mx-auto max-w-5xl px-6 py-10 animate-fade-in-up">
      <PersonalHeader
        publicKey={publicKey}
        refreshing={refreshing}
        onRefresh={handleRefresh}
      />

      <BalancesSection
        balances={balances}
        loading={balancesLoading}
        error={balancesError}
        onRetry={loadBalances}
      />

      <VestingSection
        vestingContractId={vestingContractId}
        onVestingContractChange={setVestingContractId}
        onLookup={lookupVesting}
        loading={vestingLoading}
        error={vestingError}
        schedules={vestingSchedules}
        currentLedger={currentLedger}
      />

      <OutgoingAllowancesSection onRefresh={handleRefresh} />

      {/* Burn Tokens Section */}
      <section aria-label="Burn tokens" className="mb-10">
        <h2 className="mb-4 text-sm font-medium uppercase tracking-wider text-gray-500">
          Token Actions
        </h2>
        <div className="glass-card p-6 flex flex-col md:flex-row gap-6 hover:border-red-500/20 transition-all duration-300">
          <div className="flex-1 space-y-4">
            <div className="flex items-center gap-2 text-red-400 mb-2">
              <Flame className="w-5 h-5" />
              <h3 className="font-bold text-lg">Burn My Tokens</h3>
            </div>
            <p className="text-sm text-gray-400 leading-relaxed">
              Permanently remove tokens from your balance. This action is irreversible and will reduce the total supply.
            </p>

            {burnError && (
              <div className="p-3 bg-red-400/10 border border-red-400/20 rounded-lg flex items-center gap-2 text-red-400 text-xs">
                <AlertCircle className="w-4 h-4 shrink-0" />
                <span>{burnError}</span>
              </div>
            )}

            {burnSuccess && (
              <div className="p-3 bg-green-400/10 border border-green-400/20 rounded-lg flex items-center gap-2 text-green-400 text-xs">
                <CheckCircle2 className="w-4 h-4 shrink-0" />
                <span>Tokens burned successfully! Your balance has been updated.</span>
              </div>
            )}
          </div>

          <div className="flex-1 space-y-4 bg-white/2 p-4 rounded-xl border border-white/5">
            <div className="space-y-3">
              <div>
                <label className="text-[10px] font-bold uppercase tracking-wider text-gray-500 mb-1 block">Token Contract</label>
                <select
                  value={burnContractId}
                  onChange={(e) => setBurnContractId(e.target.value)}
                  className="w-full bg-void-900 border border-white/10 rounded-lg px-3 py-2 text-sm text-white focus:outline-none focus:ring-1 focus:ring-stellar-400/50 outline-none"
                >
                  <option value="">Select a token...</option>
                  {balances.filter(b => b.assetIssuer).map(b => (
                    <option key={b.assetIssuer} value={b.assetIssuer}>{b.assetCode} ({truncateAddress(b.assetIssuer, 6)})</option>
                  ))}
                </select>
              </div>

              <div>
                <label className="text-[10px] font-bold uppercase tracking-wider text-gray-500 mb-1 block">Amount to Burn</label>
                <Input
                  type="number"
                  value={burnAmount}
                  onChange={(e) => setBurnAmount(e.target.value)}
                  placeholder="0.00"
                  className="bg-void-900 border-white/10"
                />
              </div>
            </div>

            <Button
              onClick={handleBurn}
              disabled={burnLoading || !burnContractId || !burnAmount}
              className="w-full bg-red-500/10 hover:bg-red-500/20 border border-red-500/30 text-red-400 transition-all group"
              isLoading={burnLoading}
            >
              <Flame className="w-4 h-4 mr-2 group-hover:animate-pulse" />
              Burn Tokens
            </Button>
          </div>
        </div>
      </section>

      {/* My Deployed Tokens Section */}
      <section aria-label="My deployed tokens" className="mb-10">
        <div className="flex items-center justify-between mb-4">
          <h2 className="text-sm font-medium uppercase tracking-wider text-gray-500">
            My Deployed Tokens
          </h2>
          <div className="flex gap-2">
            <Input
              value={importContractId}
              onChange={(e) => setImportContractId(e.target.value)}
              placeholder="Contract ID (C...)"
              className="w-64 h-8 text-xs bg-void-900 border-white/10"
            />
            <Button
              onClick={handleImportToken}
              disabled={importLoading || !importContractId.trim()}
              className="h-8 px-3 text-xs"
              isLoading={importLoading}
            >
              <Plus className="w-3 h-3 mr-1" />
              Import
            </Button>
          </div>
        </div>

        {importError && (
          <p className="text-xs text-red-400 mb-4">{importError}</p>
        )}

        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
          {trackedTokens.length > 0 ? (
            trackedTokens.map((token) => (
              <div 
                key={token.contractId}
                className="glass-card p-4 flex flex-col gap-3 hover:border-stellar-400/20 transition-all group"
              >
                <div className="flex justify-between items-start">
                  <div className="flex items-center gap-3">
                    <div className="w-10 h-10 rounded-full bg-stellar-400/10 flex items-center justify-center text-stellar-400 font-bold">
                      {token.symbol[0]}
                    </div>
                    <div>
                      <h3 className="font-bold text-white">{token.name}</h3>
                      <p className="text-xs text-gray-400">{token.symbol}</p>
                    </div>
                  </div>
                  <button 
                    onClick={() => handleRemoveToken(token.contractId)}
                    className="text-gray-500 hover:text-red-400 transition-colors"
                    title="Remove from dashboard"
                  >
                    <Trash2 className="w-4 h-4" />
                  </button>
                </div>
                
                <div className="space-y-1">
                  <p className="text-[10px] uppercase text-gray-500 font-bold">Contract ID</p>
                  <p className="text-xs text-gray-300 font-mono break-all">{token.contractId}</p>
                </div>

                <div className="flex justify-between items-center pt-2 border-t border-white/5">
                  <span className="text-[10px] text-gray-500 bg-white/5 px-2 py-0.5 rounded uppercase">
                    {token.network}
                  </span>
                  <div className="flex items-center gap-3">
                    {token.vestingContractId && (
                      <button
                        onClick={() => doVestingLookup(token.vestingContractId!)}
                        className="text-xs text-stellar-400 hover:text-stellar-300 font-medium"
                        title="Check vesting schedule for this deployment"
                      >
                        Check Vesting
                      </button>
                    )}
                    <Link
                      href={`/dashboard/${token.contractId}`}
                      className="text-xs text-stellar-400 hover:text-stellar-300 flex items-center gap-1 group-hover:translate-x-1 transition-transform"
                    >
                      View Dashboard
                      <ExternalLink className="w-3 h-3" />
                    </Link>
                  </div>
                </div>
              </div>
            ))
          ) : (
            <div className="col-span-full">
              <EmptyState 
                title="No deployed tokens found yet"
                description="Tokens you deploy through the SoroPad launchpad will appear here automatically. Ready to launch your first one?"
                actionLabel="Deploy Your First Token"
                actionHref="/deploy"
              />
            </div>
          )}
        </div>
      </section>

      <PersonalTransactionsSection
        operations={operations}
        loading={txLoading}
        loadingMore={txLoadingMore}
        hasMore={txNextCursor !== null}
        onLoadMore={() => loadTransactions(true)}
        publicKey={publicKey}
      />
    </div>
  );
}
