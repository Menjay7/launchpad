# Token Metadata Schema

SoroPad commits a keccak256 digest of a canonical JSON document into the
on-chain metadata registry at deploy time. This page documents that schema so
any third party can reproduce the digest from raw form values and verify it
independently against `MetadataRecord.metadata_digest`.

## Canonical JSON format

```json
{
  "name": "My Token",
  "symbol": "MTK",
  "description": "Optional – omitted when blank",
  "logoUrl": "Optional – omitted when blank",
  "website": "Optional – omitted when blank",
  "twitter": "Optional – omitted when blank",
  "discord": "Optional – omitted when blank"
}
```

### Rules

| Rule | Detail |
|------|--------|
| **Key order** | Keys appear in the order shown above. `name` and `symbol` are always present; optional keys are included only when the form value is non-empty after trimming. |
| **No nulls** | Absent optional fields are omitted entirely, not serialised as `null`. |
| **Trimmed values** | Leading/trailing whitespace is stripped from all optional values before serialisation. |
| **Encoding** | UTF-8. `JSON.stringify` default — no pretty-printing, no trailing newline. |

### Reproducing the digest

```ts
import { createHash } from "crypto"; // Node.js / edge runtime

const doc: Record<string, string> = { name, symbol };
if (description?.trim()) doc.description = description.trim();
if (logoUrl?.trim())     doc.logoUrl     = logoUrl.trim();
if (website?.trim())     doc.website     = website.trim();
if (twitter?.trim())     doc.twitter     = twitter.trim();
if (discord?.trim())     doc.discord     = discord.trim();

const json   = JSON.stringify(doc);
const digest = createHash("keccak256").update(json).digest("hex");
// digest === MetadataRecord.metadata_digest (hex-encoded BytesN<32>)
```

## On-chain record (`MetadataRecord`)

```rust
pub struct MetadataRecord {
    pub symbol:          String,        // in the clear (≤ 32 bytes)
    pub name_digest:     BytesN<32>,    // keccak256(name)
    pub creator:         Address,       // deployer's address
    pub launch_ledger:   u32,           // ledger sequence at deploy
    pub initial_supply:  i128,          // raw base units
    pub metadata_digest: Option<BytesN<32>>, // keccak256(canonical JSON), or None
}
```

`metadata_digest` is `None` when the deployer supplied no metadata (e.g. via
the legacy deploy path or the contract's `deploy_token` called directly without
a `metadata` field).

## Querying the registry

```ts
// Enumerate all launches (paginated)
registry.get_records_paginated(start: u32, limit: u32) -> Vec<IndexedLaunch>

// Fetch a specific token's record
registry.get_record(token: Address) -> Option<MetadataRecord>
```

The registry address for each network is available from the factory:

```ts
factory.get_metadata_registry() -> Option<Address>
```

## Off-chain document

The canonical JSON can be stored anywhere — IPFS (preferred), Arweave, or any
HTTPS endpoint the creator controls. When a `contract_uri` is supplied at
deploy time it is stored in the token's own `ContractUri` instance-storage key
(`DataKey::ContractUri`) and is readable via `token.contract_uri()`.

The registry commits to the **content** (via `metadata_digest`) not the
location, so the document remains verifiable even if the hosting URL changes.
