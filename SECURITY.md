# Security Policy

## Reporting a vulnerability

If you believe you have found a security vulnerability in SoroPad, please report it privately. Do not open a public GitHub issue for a suspected vulnerability.

- Email: security@soropad.dev
- Please include a description of the issue, reproduction steps, affected contracts and networks, and any suggested mitigation.

We aim to acknowledge reports within 48 hours and to provide an initial assessment within 7 days. We will keep you informed as the issue is investigated and resolved.

## Scope

This policy covers the SoroPad smart contracts, the web application in this repository, and the deployment tooling used to launch tokens.

It does not cover third-party wallets, exchanges, bridges, or other external services that users may interact with through SoroPad.

## Out of scope

- Social engineering or phishing attacks against team members.
- Denb‑of ‑service attacks that require overwhelming traffic against hosted infrastructure.
- Vulnerabilities in third-party dependencies that are already publicly disclosed and have a upstream fix available.

## Disclosure policy

We ask that you give us a reasonable window to investigate and patch an issue before you disclose it publicly. We will coordinate with you on the timing and content of any public disclosure and will credit you for the report unless you request otherwise.

## Safe harbor for researchers

We will not pursue legal action against researchers who act in good faith and follow this policy, including testing on local development networks or testnets, and who do not access, modify, or destroy data that does not belong to them.

## Hardening recommendations for operators

If you are deploying tokens with SoroPad, keep the following in mind:

- Verify the contract addresses and chain ID before signing any transaction.
- Review the ownership and upgrade path in `docs/contract-upgrade.md` so you know who can change the contract after launch.
- Review the compliance hook in `docs/compliance-node-interface.md` and the solvency model in `docs/vesting-solvency.md` before deploying.
- Treat your deployer key as a high-value secret and use a hardware wallet where possible.

## Supported versions

We provide security fixes for the latest release of the contracts and the web application. Older releases may not receive fixes.
