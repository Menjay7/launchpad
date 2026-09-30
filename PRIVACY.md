# Privacy Policy

_Last updated: 2025-01-01._

SoroPad is an open-source interface for deploying and managing SEP-41 tokens on the Stellar Soroban smart contract platform. This policy explains what the interface does with information when you use it.

The short version: SoroPad has no accounts, no server-side user profiles, and no analytics. The interface runs in your browser and talks directly to the Stellar network and to the Freighter wallet extension. Everything you sign is public on-chain by design.

---

## 1. What SoroPad does not collect

- No accounts, email addresses, or passwords.
- No names, phone numbers, or postal addresses.
- No private keys or seed phrases. SoroPad never asks for them and cannot receive them.
-No tracking cookies, advertising identifiers, or third-party analytics scripts.
- No server-side database of user activity.

## 2. What the interface keeps in your browser

The following is stored locally in your browser and is not transmitted to SoroPad:

- Your selected network (testnet or mainnet).
- The address of the wallet you connected, so the interface can show you your own tokens and vesting schedules.
- Non-sensitive UI preferences.

Clearing your browser storage or using a private window removes this data.

## 3. What goes on-chain

When you deploy a token, mint, burn, transfer, assign vesting, or transfer ownership, the transaction is signed by Freighter and submitted to the Stellar Soroban network. The following becomes publicly visible on the ledger forever:

- Your public address.
- The contract addresses involved.
- The amounts, parameters, and function calls in the transaction.
- The timestamp and ledger number.

This is a property of a public blockchain, not a feature of SoroPad. SoroPad cannot delete, alter, or hide on-chain data.

## 4. Third-party services {#third-parties} 

The interface contacts the following services directly from your browser. Their own privacy policies apply to the data they receive:

- **Stellar Soroban RPC and Horizon endpoints** - to read ledger state and submit transactions. These endpoints see your IP address and the transactions you submit.
- **Freighter wallet extension** - to connect your address and request signatures. Freighter handles key material entirely within the extension.
- **Hosting provider** - to serve the static interface files. The host sees request metadata such as IP address and user agent, as with any website.

SoroPad does not control these services and does not receive the data they process.

## 5. Legal basis and your rights (GDPR)

If you are in the European Economic Area, the General Data Protection Regulation (GDPR) applies. Because SoroPad collects no personal data on its own systems, there is no data controller processing of your personal data to exercise rights against. The processing that does occur (connecting to RJPC endpoints, signing through Freighter, and publishing to the public ledger) is initiated by and controlled by you.

On-chain data is immutable by design and cannot be erased to satisfy a right to erasure. This is a fundamental characteristic of public blockchains and you should factor it into any decision to publish data on-chain.

## 6. Children {#children}

SoroPad is not intended for use by anyone under the age of 18. It does not knowingly collect information from children.

## 7. Changes to this policy

Changes are made in the public repository and are visible in the git history of this file. The "Last updated" date at the top of this document reflects the most recent revision.

## 8. Contact

Questions about this policy can be raised as an issue in the public repository. See [`SECURITY.md`](SECURITY.md) for security-specific reporting.
