import { Control, useWatch } from "react-hook-form";
import { DeployFormData } from "../DeployForm";
import { useWallet } from "@/app/hooks/useWallet";
import { useNetwork } from "@/app/providers/NetworkProvider";
import * as StellarSdk from "@stellar/stellar-sdk";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Wallet, Zap } from "lucide-react";
import { LaunchAttestationPanel } from "@/components/ui/LaunchAttestation";

interface StepProps {
    control: Control<DeployFormData>;
    estimatedFee?: string | null;
    feeEstimationLoading?: boolean;
    feeEstimationError?: string | null;
    /** Pre-generated deploy salt — shown in the attestation panel and used on submit. */
    salt?: Uint8Array;
    /** Called with `false` when attestation detects a WASM mismatch. */
    onAttestationResult?: (ok: boolean) => void;
}

const formatIntegerString = (value?: string | number) => {
    if (value === undefined || value === null || value === "") return undefined;
    const str = typeof value === "number" ? String(value) : value;
    return str.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
};

const SummaryItem = ({ label, value }: { label: string; value: string | number | undefined }) => (
    <div className="flex justify-between py-2 border-b border-white/5">
        <span className="text-gray-400 text-sm">{label}</span>
        <span className="text-white font-medium truncate ml-4 max-w-[200px]">
            {value || <span className="text-gray-600 italic">Not set</span>}
        </span>
    </div>
);

const FlagItem = ({ label, enabled }: { label: string; enabled: boolean }) => (
    <div className="flex justify-between py-2 border-b border-white/5">
        <span className="text-gray-400 text-sm">{label}</span>
        <span className={`text-xs font-semibold px-2 py-0.5 rounded-full ${enabled ? "bg-stellar-500/20 text-stellar-300" : "bg-void-700/60 text-gray-500"}`}>
            {enabled ? "Enabled" : "Disabled"}
        </span>
    </div>
);

export const StepReview = ({ control, estimatedFee, feeEstimationLoading, feeEstimationError, salt, onAttestationResult }: StepProps) => {
    const formData = useWatch({ control });
    const { publicKey, connect } = useWallet();
    const { networkConfig } = useNetwork();
    const factoryAddress = process.env.NEXT_PUBLIC_FACTORY_ADDRESS ?? "";
    const useLegacy = process.env.NEXT_PUBLIC_USE_LEGACY_DEPLOY === "true";
    const adminModeLabel =
        formData.adminMode === "wallet"
            ? "Connected wallet"
            : "Existing multisig / DAO contract";

    return (
        <div className="space-y-6 animate-fade-in-up">
            <div className="text-left">
                <h2 className="text-xl font-bold text-white mb-2">Review & Deploy</h2>
                <p className="text-sm text-gray-400">
                    Final check before deploying your token to the network.
                </p>
            </div>
            {/* Friendbot banner: shows when on testnet and balance is low */}
            <FriendbotBanner threshold={100} />

            {/* WASM Attestation panel — shown only on the factory path */}
            {!useLegacy && factoryAddress && publicKey && salt && (
                <div>
                    <h3 className="text-xs font-bold uppercase tracking-wider text-stellar-500 mb-2">
                        Code Attestation
                    </h3>
                    <LaunchAttestationPanel
                        factoryAddress={factoryAddress}
                        deployer={publicKey}
                        salt={salt}
                        networkConfig={networkConfig}
                        onResult={onAttestationResult}
                    />
                </div>
            )}

            <div className="glass-card p-6 space-y-1">
                <h3 className="text-xs font-bold uppercase tracking-wider text-stellar-500 mb-4">Configuration Summary</h3>
                <SummaryItem label="Token Name" value={formData.name} />
                <SummaryItem label="Symbol" value={formData.symbol} />
                <SummaryItem label="Decimals" value={formData.decimals} />
                <SummaryItem
                    label="Initial Supply"
                    value={formatIntegerString(formData.initialSupply)}
                />
                <SummaryItem
                    label="Max Supply"
                    value={formData.maxSupply !== undefined ? formatIntegerString(formData.maxSupply) : "Unlimited"}
                />
                <SummaryItem label="Admin Type" value={adminModeLabel} />
                <SummaryItem label="Admin Address" value={formData.adminAddress} />
                <SummaryItem label="Compliance Node" value={formData.complianceNodeAddress || "None"} />
                <FlagItem label="Authorization Required" enabled={!!formData.authorizationRequired} />
                <FlagItem label="Authorization Revocable" enabled={!!formData.authorizationRevocable} />
                <SummaryItem label="Description" value={formData.description} />
                <SummaryItem label="Website" value={formData.website} />
                <SummaryItem label="Logo URL" value={formData.logoUrl} />
                <SummaryItem label="Twitter" value={formData.twitter} />
                <SummaryItem label="Discord" value={formData.discord} />
            </div>

            {/* Estimated Network Fee */}
            <div className="glass-card p-4 border-stellar-400/20">
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-3">
                  <div className="p-2 rounded-lg bg-stellar-400/10">
                    <Zap className="h-5 w-5 text-stellar-400" />
                  </div>
                  <div>
                    <p className="text-sm font-medium text-white">Estimated Network Fee</p>
                    <p className="text-xs text-gray-400">Pre-flight simulation cost</p>
                  </div>
                </div>
                <div className="text-right">
                  {feeEstimationLoading ? (
                    <div className="flex items-center gap-2">
                      <div className="animate-spin h-4 w-4 border-2 border-stellar-400 border-t-transparent rounded-full" />
                      <span className="text-sm text-gray-400">Estimating...</span>
                    </div>
                  ) : feeEstimationError ? (
                    <span className="text-sm text-red-400">Failed to estimate</span>
                  ) : estimatedFee ? (
                    <div className="flex items-center gap-1">
                      <span className="text-xl font-bold text-stellar-400">~{estimatedFee}</span>
                      <span className="text-sm text-gray-400">XLM</span>
                    </div>
                  ) : (
                    <span className="text-sm text-gray-500">Run &quot;Check&quot; to estimate</span>
                  )}
                </div>
              </div>
            </div>

            {(formData.authorizationRequired || formData.authorizationRevocable) && (
                <div className="bg-blue-500/10 border border-blue-500/20 p-4 rounded-xl">
                    <p className="text-xs text-blue-200 leading-relaxed">
                        <strong>Regulated token:</strong> Authorization Required is enabled.
                        Each address must be authorized before it can receive tokens — use the
                        Holder Authorization card in the token&apos;s Admin Console after deploying.
                        {formData.authorizationRevocable && " Holder authorization can be revoked by the admin at any time."}
                        {formData.complianceNodeAddress && " Transfers will also be checked against the configured compliance node."}
                    </p>
                </div>
            )}
            
            <p className="text-[10px] text-gray-500 text-center px-4">
                By deploying, you will initiate a transaction on the Stellar network. 
                Resource fees will be calculated during the pre-flight check.
            </p>

            {!publicKey && (
                <div className="p-6 rounded-xl bg-stellar-500/5 border border-stellar-500/20 text-center space-y-4 animate-pulse-subtle">
                    <div className="flex justify-center">
                        <div className="p-3 rounded-full bg-stellar-500/10">
                            <Wallet className="w-6 h-6 text-stellar-400" />
                        </div>
                    </div>
                    <div>
                        <h4 className="text-white font-bold">Wallet Disconnected</h4>
                        <p className="text-sm text-gray-400">Connect your wallet to finalize deployment.</p>
                    </div>
                    <Button 
                        onClick={connect}
                        className="w-full bg-stellar-500 hover:bg-stellar-600 text-white font-bold py-3 shadow-lg shadow-stellar-500/20"
                    >
                        Connect Wallet to Deploy
                    </Button>
                </div>
            )}
        </div>
    );
};


function FriendbotBanner({ threshold = 100 }: { threshold?: number }) {
    const { publicKey } = useWallet();
    const { networkConfig } = useNetwork();
    const [balance, setBalance] = useState<number | null>(null);
    const [loading, setLoading] = useState(false);

    const showErrorToast = (message: string) => {
        if (typeof window !== "undefined") {
            const bridge = (window as unknown as {
                __soropadToast?: { show: (t: { title: string; message?: string; variant?: "info" | "success" | "warning" | "error" }) => string };
            }).__soropadToast;
            if (bridge) {
                bridge.show({
                    title: "Friendbot funding failed",
                    message,
                    variant: "error",
                });
                return;
            }
        }

        console.error(message);
    };

    useEffect(() => {
        let cancelled = false;
        if (!publicKey) return;
        const server = new StellarSdk.Horizon.Server(networkConfig.horizonUrl);

        (async function fetchBalance() {
            try {
                const account = await server.accounts().accountId(publicKey).call();
                const native = account.balances.find((b) => b.asset_type === "native");
                if (!cancelled) setBalance(native ? Number(native.balance) : 0);
            } catch {
                // account may not exist yet
                if (!cancelled) setBalance(0);
            }
        })();

        return () => {
            cancelled = true;
        };
    }, [publicKey, networkConfig.horizonUrl]);

    if (!publicKey) return null;
    if (networkConfig.network !== "testnet") return null;
    if (balance !== null && balance >= threshold) return null;

    const fundWithFriendbot = async () => {
        try {
            setLoading(true);
            const res = await fetch(`https://friendbot.stellar.org/?addr=${encodeURIComponent(publicKey)}`);
            if (!res.ok) throw new Error("Friendbot request failed");
            // refetch balance
            const server = new StellarSdk.Horizon.Server(networkConfig.horizonUrl);
            const account = await server.accounts().accountId(publicKey).call();
            const native = account.balances.find((b) => b.asset_type === "native");
            setBalance(native ? Number(native.balance) : 0);
        } catch (err) {
            const message = err instanceof Error ? err.message : "See console for details.";
            showErrorToast(message);
        } finally {
            setLoading(false);
        }
    };

    return (
        <div className="mb-4 p-4 rounded-lg bg-yellow-900/10 border border-yellow-500/10">
            <div className="flex items-center justify-between gap-4">
                <div>
                    <p className="font-semibold text-sm text-yellow-200">Low testnet balance</p>
                    <p className="text-xs text-yellow-100">Your connected wallet has {balance === null ? 'an unknown amount of' : `${balance.toFixed(4)} XLM`}. Fund with Friendbot to proceed.</p>
                </div>
                <div>
                    <Button onClick={fundWithFriendbot} disabled={loading} className="px-4 py-2">{loading ? 'Funding…' : 'Fund with Friendbot'}</Button>
                </div>
            </div>
        </div>
    );
}
