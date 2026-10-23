# 00 — Lucky Lode at a Glance

Lucky Lode (ticker **LKYL**) is a fair-launch, hybrid PoW/PoS
cryptocurrency designed to be mined on the hardware people already own —
phones, miniPCs, and old and new laptops — with mining structured as a
game worth playing rather than a chore to automate. The chain is
transparent-first: no shielded pools are used, and every game payout is
auditable on-chain.

## The coin

- **No premine, no ICO, no team allocation.** The genesis seeds are
  consensus-locked balances with no keys and no spendable outputs: a
  **game fund** (jackpot + pot) that pays miners, a **prize fund** that
  pays Luckynode Draw winners, and **faucet/dev reserves** that pay
  capped operational claims. Every satoshi leaves through the rules —
  verifiable from genesis by any node.
- **Emission:** 60-second blocks. Week one ramps linearly from 0 to 16
  LKYL per block (no block-1 spike to front-run); the reward halves
  every two years (16 → 8 → 4 → 2), then pays a fixed **1 LKYL per
  block forever** — miners never face a fees-only future. Roughly
  31.6M LKYL over the first eight years, plus ~526k per year of tail.
- **Hybrid consensus:** 50% PoW / 50% PoS by design from genesis
  (consensus accepts either block type at any height). In practice a
  fresh chain is PoW-only until coins mature (coinbase maturity is 100
  blocks); after that either block type validates. The proof of
  work is **LuckyHash**, a purpose-built Argon2id construction (1 MiB,
  height-bound salt, frozen public test vectors — doc 05), chosen for
  low-end hardware over raw speed. Staking mints from the node wallet;
  **luckynodes** are on-chain `ln-*` identities whose heartbeats enter
  the consensus Luckynode Draw — no bond, no pool custody, paid from
  the prize fund, never from inflation.
- **Game layer (funded by a 5% block levy, expectation-neutral by
  construction):**
  - **Jackpot blocks** (~1 per 100): the finder earns a bonus up to
    5x the block reward, capped by a publicly tracked reserve.
  - **The Lucky Pot:** tiered draws pay splashes of 10/25/50% of the
    pot on pre-committable, verifiable draw outcomes.
  - **Luck classes:** every block lands in one of ten themed classes
    (mythic, aurora, comet, beacon, bonfire, cinder, ember, flare,
    glint, spark) — a display/flavor layer, not consensus: classes
    fund nothing today.
  - **Divisions and streaks** run pool-side on the same auditable
    ledger; the **Luckynode Draw** is consensus money (doc 06 §6.7) —
    stakers enter it with on-chain `ln-*` identities rather than
    pool registration.

## Status

A public testnet is live: chain, faucet, reference pool, block
explorer, and desktop wallet are deployed at `luckylode.cc`. Mainnet
launch follows the roadmap and gates in doc 04.

## Where things live

| Component | Repository |
|---|---|
| Node (C++, Verus v1.2.18 fork) | [LuckyLode-node](https://github.com/kdsmith18542/LuckyLode-node) |
| Miner, pool, wallet core, explorer, faucet, tooling | This repository ([LuckyLode](https://github.com/kdsmith18542/LuckyLode)) |
| Design documents (this directory) | `docs/01`–`docs/13` |

Start with the [design record index](README.md) for the full documents,
or the [thesis](01-thesis-and-positioning.md) for why this niche is
open and what killed its predecessors.
