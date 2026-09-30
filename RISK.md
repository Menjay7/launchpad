# Risk Disclosure

_Last updated: 2025-01-01. This notice applies to every page of the SoroPad interface and to every transaction initiated through it._

SoroPad is an open-source interface for deploying and managing SEP-41 tokens on the Stellar Soroban smart contract platform. Before you deploy a token, mint supply, assign vesting, or transfer ownership, read this notice in full.

__Transactions are submitted through your wallet and may be irreversible. Tokens can be volatile or lose all value. SoroPad does not provide custody, warranties, or financial advice.__

---

## 1. You are the operator

SoroPad is software. It is not a broker, dealer, exchange, custodian, transfer agent, or fiduciary. When you deploy a token, you are the issuer and the operator of that token. You are responsible for every legal, tax, regulatory, and consumer-protection obligation that attaches to issuing a token to the public, including in your jurisdiction and in the jurisdictions of the people who acquire it.

SoroPad does not advise on, monitor, or enforce compliance with any regulatory regime. The compliance hook described in [`docs/compliance-node-interface.md](docs/compliance-node-interface.md) is a technical integration point. It is not a compliance program, and its presence does not make a token compliant in any jurisdiction.

## 2. No financial advice

Nothing in this repository, the interface, or any related documentation is financial, investment, legal, tax, or accounting advice. SoroPad does not recommend any token, does not endorse any issuer, and does not evaluate the merits of any launch. Consult qualified professionals before issuing or acquiring any token.

## 3. No custody

SoroPad never holds your assets, your keys, or your funds. Transactions are constructed in your browser and signed by the wallet you control (currently Freighter). SoroPad cannot sign for you, cannot reverse a signed transaction, and cannot recover assets sent to a wrong address or a wrong contract.

## 4. Irreversibility

Soroban transactions are final once included in a ledger. Deployments, mints, burns, transfers, vesting assignments, and ownership transfers cannot be undone by SoroPad. If you deploy with the wrong parameters, mint to the wrong address, or transfer ownership to a key you do not control, the consequences are permanent unless the contract itself provides a recovery path and the current owner executes it.

## 5. Token value and liquidity

SoroPad does not create a market. There is no liquidity pool, swap, or price quote in this software. A token deployed through SoroPad may have no market, no buyers, and no price. Token prices are volatile and can fall to zero. Never spend more than you can afford to lose entirely.

## 6. What the owner can do after launch

A SEP-41 token deployed through SoroPad exposes administrative functions to whoever holds the owner role. The current owner can, at minimum, mint new supply (up to the configured max cap), burn supply, and transfer ownership to another address. The owner can also assign or modify vesting schedules where the vesting contract allows it. The exact set of administrative functions is defined by the deployed contract code and by the contract upgrade rules in [`docs/contract-upgrade.md](docs/contract-upgrade.md).

Because the owner can mint, a token holder faces dilution risk. Because the owner can transfer ownership, the governance of the token can change without holder consent. Because contracts may be upgradable under the rules in [`docs/contract-upgrade.md`](docs/contract-upgrade.md), the behaviour of a token can change after you acquire it. Read those documents before deploying or acquiring.

## 7. Vesting and solvency

Vesting schedules are on-chain commitments. The solvency model described in [`docs/vesting-solvency.md`](docs/vesting-solvency.md) is a disclosure mechanism, not a guarantee. A vesting schedule can be funded or unfunded. An unfunded schedule may never pay out. SoroPad does not guarantee that any vesting obligation will be met.

## 8. Smart contract risk {#smart-contract-risk}

Smart contracts can contain bugs. The Soroban runtime, the Stellar network, the Freighter wallet, and the browser environment are all outside SoroPad's control. A defect in any of them can result in lost funds or incorrect state. SoroPad provides no warranty of correctness, availability, or fitness for a particular purpose. See [`SECURITY.md`](SECURITY.md) for how to report a vulnerability.

## 9. Regulatory exposure

If you are located in the European Union, or if you offer a token to persons in the EU, the Markets in Crypto-Assets Regulation (MiCA) and related national laws may apply to you. MiCA imposes obligations on crypto-asset issuers and service providers, including whitepaper, disclosure, and authorisation requirements depending on the type of asset and the service offered. SoroPad does not provide a MiCA whitepaper, does not provide a MiCA assessment, and does not authorise any launch. Obtaining a MiCA assessment is legal work that you must commission yourself.

## 10. No representation of approval

Nothing in this repository or the interface should be read as an approval, endorsement, or verification of any token, issuer, or launch. The fact that a token was deployed through SoroPad carries no weight as to its legality, safety, or value.

## 11. Acceptance

By deploying a token, minting supply, assigning vesting, or transferring ownership through SoroPad, you confirm that you have read and understood this notice, that you accept the risks described above, and that you accept the [Terms of Use](TERMS.md) and the [Privacy Policy](PRIVACY.md).
