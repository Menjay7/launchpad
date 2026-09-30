import * as StellarSdk from "@stellar/stellar-sdk";
import type { NetworkConfig } from "../types/network";
import { fetchTokenInfo } from "./stellar";

export interface TrackedDeployment {
  contractId: string;
  name: string;
  symbol: string;
  network: string;
  timestamp: number;
  vestingContractId?: string;
}

// UI state only - not the source of truth for what exists
const UI_STATE_KEY_PREFIX = "soropad:ui_state:";

export interface DeploymentUiState {
  pinned: string[];
  dismissed: string[];
  lastVisited: Record<string, number>;
}

export function getUiState(walletAddress: string): DeploymentUiState {
  if (typeof window === "undefined") return { pinned: [], dismissed: [], lastVisited: {} };
  try {
    const key = `${UI_STATE_KEY_PREFIX}${walletAddress}`;
    const stored = localStorage.getItem(key);
    if (!stored) return { pinned: [], dismissed: [], lastVisited: {} };
    return JSON.parse(stored) as DeploymentUiState;
  } catch (e) {
    console.error("Failed to get UI state", e);
    return { pinned: [], dismissed: [], lastVisited: {} };
  }
}

export function setUiState(walletAddress: string, state: DeploymentUiState) {
  if (typeof window === "undefined") return;
  try {
    const key = `${UI_STATE_KEY_PREFIX}${walletAddress}`;
    localStorage.setItem(key, JSON.stringify(state));
  } catch (e) {
    console.error("Failed to set UI state", e);
  }
}

export function setLastVisited(walletAddress: string, contractId: string) {
  const state = getUiState(walletAddress);
  state.lastVisited[contractId] = Date.now();
  setUiState(walletAddress, state);
}

export function togglePinned(walletAddress: string, contractId: string) {
  const state = getUiState(walletAddress);
  const index = state.pinned.indexOf(contractId);
  if (index > -1) {
    state.pinned.splice(index, 1);
  } else {
    state.pinned.push(contractId);
  }
  setUiState(walletAddress, state);
}

export function dismissDeployment(walletAddress: string, contractId: string) {
  const state = getUiState(walletAddress);
  if (!state.dismissed.includes(contractId)) {
    state.dismissed.push(contractId);
  }
  setUiState(walletAddress, state);
}

/**
 * Fetch all deployments for a given wallet from the factory.
 * This is the source of truth for what exists, not localStorage.
 */
export async function fetchWalletDeployments(
  walletAddress: string,
  config: NetworkConfig,
  factoryAddress: string,
): Promise<TrackedDeployment[]> {
  if (!factoryAddress) return [];

  try {
    const rpc = new StellarSdk.rpc.Server(config.rpcUrl);
    const account = new StellarSdk.Account(StellarSdk.Keypair.random().publicKey(), "0");

    // Get total deployment count
    const countTx = new StellarSdk.TransactionBuilder(account, {
      fee: StellarSdk.BASE_FEE,
      networkPassphrase: config.passphrase,
    })
      .addOperation(new StellarSdk.Contract(factoryAddress).call("get_deployment_count"))
      .setTimeout(30)
      .build();

    const countSim = await rpc.simulateTransaction(countTx);
    if (!StellarSdk.rpc.Api.isSimulationSuccess(countSim) || !countSim.result) {
      return [];
    }

    const count = Number(StellarSdk.scValToNative(countSim.result.retval));
    if (count === 0) return [];

    // Fetch all deployments (paginated, max 100 per call, but we need all)
    const allAddresses: string[] = [];
    const pageSize = 100;
    for (let start = 0; start < count; start += pageSize) {
      const limit = Math.min(pageSize, count - start);
      const paginatedTx = new StellarSdk.TransactionBuilder(account, {
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

      const paginatedSim = await rpc.simulateTransaction(paginatedTx);
      if (StellarSdk.rpc.Api.isSimulationSuccess(paginatedSim) && paginatedSim.result) {
        const addresses = StellarSdk.scValToNative(paginatedSim.result.retval) as string[];
        allAddresses.push(...addresses);
      }
    }

    // Fetch token info for each deployment and filter by deployer
    const deployments: TrackedDeployment[] = [];
    const results = await Promise.allSettled(
      allAddresses.map((addr) => fetchTokenInfo(addr, config)),
    );

    results.forEach((result, i) => {
      if (result.status === "fulfilled") {
        const info = result.value;
        // The deployer is the admin (deploy_token passes deployer as admin)
        if (info.admin === walletAddress) {
          deployments.push({
            contractId: allAddresses[i],
            name: info.name,
            symbol: info.symbol,
            network: config.network,
            timestamp: Date.now(), // Could be enhanced with event timestamp
          });
        }
      }
    });

    return deployments;
  } catch (e) {
    console.error("Failed to fetch wallet deployments", e);
    return [];
  }
}

// Legacy functions for backward compatibility - deprecated
const STORAGE_KEY_PREFIX = "soropad:deployments:";

export function getTrackedDeployments(walletAddress: string): TrackedDeployment[] {
  if (typeof window === "undefined") return [];
  try {
    const key = `${STORAGE_KEY_PREFIX}${walletAddress}`;
    const stored = localStorage.getItem(key);
    if (!stored) return [];
    return JSON.parse(stored) as TrackedDeployment[];
  } catch (e) {
    console.error("Failed to get tracked deployments", e);
    return [];
  }
}

export function trackDeployment(
  walletAddress: string,
  deployment: Omit<TrackedDeployment, "timestamp">
) {
  if (typeof window === "undefined") return;
  try {
    const key = `${STORAGE_KEY_PREFIX}${walletAddress}`;
    const current = getTrackedDeployments(walletAddress);
    
    // Avoid duplicates
    if (current.some(d => d.contractId === deployment.contractId)) {
      return;
    }

    const updated = [
      { ...deployment, timestamp: Date.now() },
      ...current
    ];
    
    localStorage.setItem(key, JSON.stringify(updated));
  } catch (e) {
    console.error("Failed to track deployment", e);
  }
}

export function removeTrackedDeployment(walletAddress: string, contractId: string) {
  if (typeof window === "undefined") return;
  try {
    const key = `${STORAGE_KEY_PREFIX}${walletAddress}`;
    const current = getTrackedDeployments(walletAddress);
    const updated = current.filter(d => d.contractId !== contractId);
    localStorage.setItem(key, JSON.stringify(updated));
  } catch (e) {
    console.error("Failed to remove tracked deployment", e);
  }
}
