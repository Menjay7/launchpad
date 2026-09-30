# Vesting Solvency

The vesting contract keeps a live `total_committed` counter for tokens still
owed to active schedules. The counter increases when schedules are created and
decreases when tokens are released or a schedule is revoked.

Recipients and auditors can call `solvency()` to compare that commitment total
with the vesting contract's current token balance:

- `token_balance`: live balance held by the vesting contract.
- `total_committed`: amount still owed to active schedules.
- `solvent`: `true` when `token_balance >= total_committed`.

The token admin can still undermine a vesting contract if they retain token
admin powers. For example, a token with clawback enabled can claw tokens back
from the vesting contract address after schedules are funded. A paused or
frozen token can also prevent otherwise valid releases from completing.

For grants that need strong credibility, use one of these patterns:

- Call `revoke_admin` on the token contract after funding grants, if the token
  no longer needs admin-managed minting, clawback, pause, or freeze powers.
- Use a separate token admin that is independent from the vesting admin.
- Prefer multisig or governed admin accounts for high-value grant programs.

The solvency badge in the frontend is a trust signal, not an enforcement layer.
It shows whether the vesting contract is currently funded enough to cover its
recorded commitments, so recipients can spot underfunding before attempting a
 claim.

## Continuous Streams

Continuous streams extend the same accounting model to a per-ledger vesting
schedule. A claim on a stream is a pull payment: the recipient signs a claim
authorisation off-chain, a relayer submits it, and the contract pays out the
amount that has accrued since the last release.

The contract cannot verify an arbitrary signature because Soroban has no
`ecrecover`. Instead, a claim is authorised with a scoped
Soroban authorization entry. The recipient pre-authorises a bounded claim
range once, and the contract enforces the scope on every submission.

The stream API is:

- `stream_grant(recipient, total, start_ledger, end_ledger, max_per_claim)`
  writes a `Stream` schedule and increases `total_committed` by `total`.
- `claim_stream(recipient, auth)` is the relayed entry point. It verifies that
  the authorisation covers `(recipient, from_ledger, to_ledger)`, that
  `to_ledger <= end_ledger`, and that the payout does not exceed
  `max_per_claim`. It then pays `accrued(to_ledger) - released` and reduces
  `total_committed` by the same amount.
- `stream_accrued(recipient)` is a pure arithmetic view. It never touches the
  token contract, advances no ledger state, and can be queried by anyone.

The `max_per_claim` bound is what keeps a single authorised invocation from
draining the contract. Because the authorisation is scoped to a bounded
`(recipient, from_ledger, to_ledger)` range, a relayer cannot replay it for a
different recipient or widen it beyond the signed bounds.

## Solvency as Enforcement

For cliff-plus-linear grants, solvency is advisory: a paused or frozen token
stops a release, but the accounting is unaffected. Continuous streams make
solvency load-bearing instead of advisory.

A stream's accrual can be read without a transaction, so a paused or frozen
token does not stop the accounting. Only the withdrawal is blocked. The
contract must therefore check `solvency()` before every stream payout and
refuse to release tokens that would push `token_balance` below
`total_committed`. The accrued amount keeps growing on schedule even while the
token is unavailable, and the recipient can claim the backlog once the token
resumes normal operation.

This is the one place where the launchpad's `solvency()` accounting becomes
load-bearing rather than advisory. The counter is no longer just a trust
signal for recipients; the contract itself uses it to decide whether a stream
claim can be paid out.
