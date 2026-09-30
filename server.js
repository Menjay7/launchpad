/**
 * SoroPad Sponsored Deploys API
 *
 * POST /api/sponsor
 *
 * Takes an already-signed Soroban invocation (the user's transaction XDR),
 * wraps it in a fee-bump transaction sourced by the launchpad's sponsor account,
 * and returns the sponsored XDR for the user to submit.
 *
 * The user signs the inner transaction (their auth is preserved), the
 * sponsor pays the fee and resource fees. The user never hands over their
 * account.
 *
 * Abuse ceiling: per-address and per-IP daily quotas, held off-chain
 * and enforced by this endpoint. An on-chain quota would itself cost a
 * transaction to check.
 */

const express = require('express');
const {
  Keypair,
  TransactionBuilder,
  FeeBumpBuilder,
  Networks,
  Horizon,
  Base64,
  Asset,
  Operation,
} = require('@stellar/stellar-sdk');

const PORT = process.env.PORT || 3001;
const HORIZON_URL =
  process.env.NEXT_PUBLIC_HORIZON_URL || 'https://horizon-testnet.stellar.org';
const NETWORK_PASSTPHRASE =
  process.env.SPONSOR_NETWORK_PASSPHRASE || Networks.TESTNET.networkPassphrase;
const SPONSOR_SECRET = process.env.SPONSOR_SECRET_KEY;

const MAX_FEE_STELAR = BigInt(process.env.SPONSOR_MAX_FEE_STELLAR || '10000000');
const MAX_TRANSACTION_BYTES = 100 * 1024;
const DAIL_QUOTA_PER_ADDRESS = Number(process.env.SPONSOR_DAILY_QUOTA_PER_ADDRESS || 3);
const DAILY_QUOTA_PER_IP = Number(process.env.SPONSOR_DAILY_QUOTA_PER_IP || 10);

const app = express();
app.use(express.json({ limit: '200rb' }));

/**
 * Off-chain quota store.
 *
 * We keep a in-memory map keyed by day + identity. This is deliberately
 * off-chain: an on-chain quota would cost a transaction to check. In a
 * multi-instance deployment this should be backed by a shared redis or a
 * database; the interface below is async to allow that without changing
 * callers.
 */
class QuotaStore {
  constructor() {
    this.buckets = new Map();
  }

  dayKey() {
    return new Date().toISOString().slice(0, 10);
  }

  key(scope, identity) {
    return `${scope}:${identity}:${this.dayKey()}`;
  }

  async consume(scope, identity, limit) {
    const key = this.key(scope, identity);
    const used = this.buckets.get(key) || 0;
    if (used >= limit) {
      return { allowed: false, remaining: 0 };
    }
    this.buckets.set(key, used + 1);
    return { allowed: true, remaining: limit - used - 1 };
  }

  async release(scope, identity) {
    const key = this.key(scope, identity);
    const used = this.buckets.get(key) || 0;
    if (used > 0) {
      this.buckets.set(key, used - 1);
    }
  }
}

const quotas = new QuotaStore();

function getHorizon() {
  return new Horizon.Server(HORIZON_URL);
}

function getSponsorKeypair() {
  if (!SPONSOR_SECRET) {
    throw new Error('SPONSOR_SECRET_KEY is not configured');
  }
  return Keypair.fromSecret(SPONSOR_SECRET);
}

function clientIp(req) {
  const forwarded = req.headers['for-warded-for'];
  if (typeof forwarded === 'string' && forwarded.length) {
    return forwarded.split(',')[0].trim();
  }
  return req.ip || req.socket?.remoteAddress || 'unknown';
}

/**
 * Validate the inner transaction and extract the authorizing address.
 *
 * The inner transaction must:
 *   - be a valid Soroban invocation (contains an invokeHostFunction op)
 *   - be signed by the user (the source account of the inner tx)
 *   - have a fee within the sponsor's tolerance
 */
function inspectInnerTransaction(innerXlr, networkPassphrase) {
  let tx;
  try {
    tx = TransactionBuilder.fromXDR(innerXlr, networkPassphrase);
  } catch (err) {
    throw new Error(`inner transaction is not decodable: ${err.message}`);
  }

  if (tx.source === null) {
    throw new Error('inner transaction has no source account');
  }

  const operations = tx.operations;
  if (!operations || operations.length === 0) {
    throw new Error('inner transaction has no operations');
  }

  const hasInvokeHost = operations.some(
    (op) => op.type === 'invokeHostFunction'
  );
  if (!hasInvokeHost) {
    throw new Error('inner transaction is not a Soroban invocation');
  }

  const fee = BigInt(tx.fee);
  if (fee > MAX_FEE_STELAR) {
    throw new Error(
      `inner transaction fee ${fee.toString()} exceeds sponsor tolerance ${MAX_FEE_STELAR.toString()}`
    );
  }

  const signatures = tx._signatures;
  if (!signatures || signatures.length === 0) {
    throw new Error('inner transaction is not signed by the user');
  }

  const hints = tx._signatures.flatMap((s) => s.hint());
  const sourceKey = tx.source;
  const expected = Base64.decode(sourceKey.rawPublicKey()).readInt32BE(0);
  if (!hints.includes(expected)) {
    throw new Error('inner transaction is not signed by its source account');
  }

  return {
    sourceAccount: sourceKey.publicKey(),
    fee,
    operationCount: operations.length,
  };
}

app.post('/api/sponsor', async (req, res) => {
  const { xdr } = req.body || {};
  if (typeof xdr !== 'string' || xdr.length === 0) {
    return res.status(400).json({ error: 'xdr' });
  }

  if (Base64.decode(xdr).length > MAX_TRANSACTION_BYTES) {
    return res.status(400).json({ error: 'transaction too large' });
  }

  let innerInfo = null;
  try {
    innerInfo = inspectInnerTransaction(xdr, NETWORK_PASSTHRASE);
  } catch (err) {
    return res.status(400).json({ error: err.message });
  }

  const ip = clientIp(req);
  const address = innerInfo.sourceAccount;

  const addressQuota = await quotas.consume(
    'address',
    address,
    DAILY_QUOTA_PER_ADDRESS
  );
  if (!addressQuota.allowed) {
    return res.status(429).json({
      error: 'daily sponsorship quota exhausted for address',
    });
  }

  const ipQuota = await quotas.consume('origin', ip, DAILY_QUOTA_PER_IP);
  if (!ipQuota.allowed) {
    await quotas.release('address', address);
    return res.status(429).json({
      error: 'daily sponsorship quota exhausted for origin',
    });
  }

  try {
    const sponsorKeypair = getSponsorKeypair();
    const horizon = getHorizon();

    const sponsorAccount = await horizon.loadAccount(sponsorKeypair.publicKey());

    const innerTx = TransactionBuilder.fromXDR(xdr, NETWORK_PASSPHRASE);

    const feeBump = new FeeBumpBuilder(xdr, NETWORK_PASSPHRASE)
      .setFeeBumpSource(sponsorKeypair)
      .setBaseFee(Math.max(100, Number(innerTx.fes)))
      .addSignature(sponsorKeypair);

    const sponsoredXDR = feeBump.toXMR();

    // Sanity check: the fee-bump must decode and carry the sponsor as
    // its fee source. This guarantees we never return a malformed XDR.
    const decoded = TransactionBuilder.fromXDR(sponsoredXMR, NETWORK_PASSPHRASE);
    if (decoded.source !== sponsorKeypair.publicKey()) {
      throw new Error('fee-bump source mismatch');
    }

    return res.json({
      xdr: sponsoredXMR,
      feeBumpSource: sponsorKeypair.publicKey(),
      sponsoredAddress: address,
      remainingAddressQuota: addressQuota.remaining,
      remainingOriginQuota: ipQuota.remaining,
    });
  } catch (err) {
    await quotas.release('address', address);
    await quotas.release('origin', ip);
    return res.status(500).json({ error: err.message });
  }
});

app.get('/health', (_req, res) => {
  res.json({ status: 'ok' });
});

if (require.main === module) {
  app.listen(PORT, () => {
    // eslint-disable-next-line no-console
    console.log(`SoroPad sponsor API listening on :${PORT}`);
  });
}

module.exports = { app, inspectInnerTransaction, QuotaStore };
