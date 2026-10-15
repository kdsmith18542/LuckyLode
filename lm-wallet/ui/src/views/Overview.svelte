<script lang="ts">
  import * as api from "../lib/api";
  let bal = $state<any>(null);
  let txs = $state<any[]>([]);
  let pool = $state<any>(null);
  let err = $state("");
  let detail = $state<any>(null);

  async function load() {
    try {
      bal = await api.walletBalances();
      txs = ((await api.transactions(25)) as any[]) ?? [];
      pool = await api.poolStats();
      err = "";
    } catch (e) {
      err = String(e);
    }
  }
  $effect(() => {
    load();
    const t = setInterval(load, 15000);
    return () => clearInterval(t);
  });

  async function openDetail(txid: string) {
    if (!txid) return;
    try {
      detail = await api.txDetail(txid);
    } catch (e) {
      err = String(e);
    }
  }

  const recent = $derived((pool?.last_blocks ?? []).slice(0, 5));
</script>

<h2>Balances</h2>
<div class="grid">
  <div class="card"><div class="k">available</div><div class="v">{api.coin(bal?.balance)}</div></div>
  <div class="card"><div class="k">staking</div><div class="v">{api.coin(bal?.staking_balance)}</div></div>
  <div class="card"><div class="k">immature</div><div class="v gold">{api.coin(bal?.immature)}</div></div>
  <div class="card"><div class="k">unconfirmed</div><div class="v">{api.coin(bal?.unconfirmed)}</div></div>
</div>

<h2>Recent activity</h2>
<table>
  <thead><tr><th>time</th><th>direction</th><th>amount</th><th>address</th><th>status</th></tr></thead>
  <tbody>
    {#each txs.slice(0, 12) as tx}
      <tr style="cursor:pointer" onclick={() => openDetail(tx.txid)} title="click for details">
        <td>{new Date((tx.time ?? 0) * 1000).toLocaleString()}</td>
        <td class:off={tx.category !== "receive"}>{tx.category}</td>
        <td>{tx.amount}</td>
        <td class="muted">{api.short(tx.address, 10)}</td>
        <td class="muted">{tx.confirmations ?? 0} conf</td>
      </tr>
    {/each}
    {#if txs.length === 0}<tr><td colspan="5" class="muted">no transactions yet</td></tr>{/if}
  </tbody>
</table>

{#if detail}
  <div class="card" style="max-width:720px">
    <div class="row" style="justify-content:space-between">
      <b>transaction detail</b>
      <button class="btn" onclick={() => (detail = null)}>close</button>
    </div>
    <table>
      <tbody>
        <tr><th>txid</th><td class="addr">{detail.txid}</td></tr>
        <tr><th>amount</th><td>{detail.amount ?? "–"} LKYL</td></tr>
        <tr><th>fee</th><td>{detail.fee ?? "–"}</td></tr>
        <tr><th>confirmations</th><td>{detail.confirmations ?? 0}</td></tr>
        <tr><th>block height</th><td>{detail.blockheight ?? "–"}</td></tr>
        <tr><th>time</th><td>{new Date((detail.time ?? 0) * 1000).toLocaleString()}</td></tr>
      </tbody>
    </table>
    {#if Array.isArray(detail.details) && detail.details.length > 0}
      <table>
        <thead><tr><th>direction</th><th>address</th><th>amount</th></tr></thead>
        <tbody>
          {#each detail.details as d}
            <tr>
              <td>{d.category}</td>
              <td class="muted">{api.short(d.address, 12)}</td>
              <td>{d.amount}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
{/if}

<h2>Pool blocks (recent)</h2>
<table>
  <thead><tr><th>height</th><th>finder</th><th>jackpot</th><th>pot</th></tr></thead>
  <tbody>
    {#each recent as b}
      <tr>
        <td>{b.height}</td>
        <td class="muted">{api.short(b.finder_name ?? b.finder, 10)}</td>
        <td>{api.coin(b.bonus)}</td>
        <td>{api.coin(b.pot)}</td>
      </tr>
    {/each}
    {#if recent.length === 0}<tr><td colspan="4" class="muted">no blocks indexed yet</td></tr>{/if}
  </tbody>
</table>
{#if err}<div class="err">{err}</div>{/if}
