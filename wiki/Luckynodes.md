# Luckynodes

Luckynodes are bonded full nodes that heartbeat to the pool, cosign the
[Luckynode Draw](Game-Layer), and enter it. The stakers' game layer.

## Run one

1. Collateral: 400 LKYL on mainnet (**0 on testnet** until coins mature).
2. Register with a payout address you prove ownership of (signed
   message); keep the bond UTXO untouched — spending it evicts you.
3. Heartbeat every epoch (3 blocks on testnet). The `luckynode` daemon
   or the wallet service does registration + heartbeat + auto-cosign.
4. Watch yourself on the pool dashboard (Luckynodes section).

## The Draw

Every epoch a pre-committed draw picks a winner from active nodes;
10% of trailing league inflow pays the winner on-chain, with cosigns
from the set. Testnet draws pay real (worthless) txs every few blocks —
perfect for watching the flow on the explorer's [game page](Game-Layer).
