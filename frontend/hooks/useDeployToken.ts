import { useCallback, useState } from "react";
import {
  Address,
  Contract,
  Scval,
  TransactionBuilder,
  nativeToScval,
  stringToScval,
  xdrToScVal,
} from "@stellar/stellar-sdk";
import { useWallet } from "./useWallet";
import { factoryClient } from "@/lib/bindings/factory";

export interface TokenConfig {
  name: string;
  symbol: string;
  decimals: number;
  initialSupply: bigint;
  mintable: boolean;
  burnable: boolean;
}

export interface DeployTokenParams {
  factoryId: string;
  config: TokenConfig;
  salt?: string;
}

export interface UseDeployTokenResult {
  deployToken: (params: DeployTokenParams) => Promise<string>;
  isPending: boolean;
  error: Error | null;
  tokenAddress: string | null;
}

function toTokenConfigScval(config: TokenConfig): ScVal {
  return xdrToScVal(
    {
      name: config.name,
      symbol: config.symbol,
      decimals: config.decimals,
      initial_supply: config.initialSupply,
      mintable: config.mintable,
      burnable: config.burnable,
    },
    [
      ["name", "string"],
      ["symbol", "string"],
      ["decimals", "u32"],
      ["initial_supply", "i128"],
      ["mintable", "bool"],
      ["burnable", "bool"],
    ],
  );
}

export function useDeployToken(): UseDeployTokenResult {
  const { address, signTransaction } = useWallet();
  const [isPending, setIsPending] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [tokenAddress, setTokenAddress] = useState<string | null>(null);

  const deployToken = useCallback(
    async ({ factoryId, config, salt }: DeployTokenParams) => {
      if (!address) {
        throw new Error("Wallet not connected");
      }
      setIsPending(true);
      setError(null);
      try {
        const contract = new Contract(factoryId);
        const client = factoryClient(contract);
        const op = client.deploy_token({
          caller: Address.fromString(address),
          config: toTokenConfigScval(config),
          salt: salt ? stringToScval(salt) : nativeToScVal(undefined),
        });
        const tx = new TransactionBuilder(contract.call("deploy_token", op)).setFee("auto");
        const signed = await signTransaction(tx);
        const result = await client.deploy_token({
          caller: Address.fromString(address),
          config: toTokenConfigScval(config),
          salt: salt ? stringToScval(salt) : nativeToScval(undefined),
        });
        const addr = result.result;
        setTokenAddress(addr);
        return addr;
      } catch (e) {
        const err = e instanceof Error ? e : new Error(String(e));
        setError(err);
        throw err;
      } finally {
        setIsPending(false);
      }
    },
    [address, signTransaction],
  );

  return { deployToken, isPending, error, tokenAddress };
}
