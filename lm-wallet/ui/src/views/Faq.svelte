<script lang="ts">
  // Static FAQ — honest answers, no marketing. Content mirrors the docs
  // (luckylode.cc + this repo's docs/) and the node's real behavior.
  const faq: [string, string][] = [
    [
      "What is this wallet connected to?",
      "By default it starts and manages your own node on this machine. You can instead point it at any Lucky Lode node's RPC (Settings → Node), including the public testnet endpoint. The node is a data source and broadcast relay — your keys never leave this wallet.",
    ],
    [
      "Where are my coins?",
      "In outputs secured by keys this wallet derives from your 12-word backup phrase (BIP39/BIP44). The node cannot spend them; a Send you preview and confirm is signed here and only the finished transaction is broadcast.",
    ],
    [
      "How do I stake?",
      "Staking mints from the node wallet, not from this app directly. Overview walks you through it: run a local node, then either import your funded keys into it (same machine, no transfer) or send coins to its wallet, wait for 150 confirmations, and Start. Coins you stake stay spendable — there is no lock, and no slashing.",
    ],
    [
      "Can this app's coins win a jackpot or a pot splash?",
      "Yes, if you mine. Jackpots and pot splashes go to the miner who finds the block (decided by a lottery ticket fixed before the block is mined). Stakers participate in the Luckynode Draw raffle instead.",
    ],
    [
      "What is a luckynode?",
      "A Luckynode Draw entrant: register an ln-* identity once, keep it active with a small update transaction each epoch, and earn tickets weighted by how long you have been registered. Each epoch's draw is paid from the prize fund. The embedded service in this app only runs while the app is open; for 24/7 participation run the standalone daemon on an always-on machine.",
    ],
    [
      "Why does the miner say shares accepted but no blocks?",
      "A share meets the pool's (lower) difficulty target; a block meets the network's. Blocks are rare by design — the pool credits every accepted share, and a block find pays the whole PPLNS window.",
    ],
    [
      "My balance went down right after sending. Why?",
      "Sends include a miner fee and move change back to your wallet. While the transaction is unconfirmed the change is not spendable; it appears again after one confirmation. The preview screen shows fee and change before you confirm.",
    ],
    [
      "Is this mainnet or testnet?",
      "The badge next to the ticker follows the connected chain: tLKYL is testnet, LKYL is mainnet, rLKYL is regtest. Testnet coins have no value.",
    ],
    [
      "I lost my password. Now what?",
      "The password encrypts the seed on this machine — it cannot be reset. Restore from the 12-word backup phrase instead (Settings → wallet → restore), with a new password.",
    ],
    [
      "Something looks wrong. Where are the logs?",
      "Settings → Node shows the managed node's log location; the pool dashboard and explorer at luckylode.cc show chain-side truth (your txid, the block, the draw).",
    ],
  ];
</script>

<h2>FAQ</h2>
<div class="card" style="max-width:820px">
  {#each faq as [q, a]}
    <details style="margin-bottom:8px">
      <summary class="copyable" style="font-weight:600">{q}</summary>
      <div class="muted" style="margin-top:6px">{a}</div>
    </details>
  {/each}
</div>
