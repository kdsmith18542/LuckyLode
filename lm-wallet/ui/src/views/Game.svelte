<script lang="ts">
  import * as api from "../lib/api";
  let info = $state<any>(null);
  let pool = $state<any>(null);
  let draw = $state<any>(null);
  let err = $state("");

  async function load() {
    try {
      info = await api.gameInfo();
      pool = await api.poolStats();
      draw = await api.nextDraw();
      err = "";
    } catch (e) {
      err = String(e);
    }
  }
  $effect(() => {
    load();
    const t = setInterval(load, 10000);
    return () => clearInterval(t);
  });

  const jp = $derived<any>(info?.jackpot ?? {});
  const mining = $derived<any>(info?.mining ?? {});
  const blocks = $derived<any[]>((pool?.last_blocks ?? []).filter((b: any) => b.bonus > 0 || b.pot > 0));
  const droughtLeft = $derived(
    typeof jp.drought === "number" && typeof jp.potdrought === "number"
      ? Math.max(0, jp.potdrought - jp.drought)
      : null,
  );
</script>

<h2>Game state</h2>
<div class="grid">
  <div class="card"><div class="k">jackpot reserve</div><div class="v gold">{api.coin(jp.reserve)}</div></div>
  <div class="card"><div class="k">lucky pot</div><div class="v">{api.coin(jp.pot)}</div></div>
  <div class="card">
    <div class="k">pot pity in</div>
    <div class="v">{droughtLeft !== null ? `${droughtLeft} blocks` : "–"}</div>
  </div>
  <div class="card"><div class="k">difficulty</div><div class="v">{mining.difficulty ?? "–"}</div></div>
</div>

<h2>Next block draw (pre-committed)</h2>
<div class="card" style="max-width:640px">
  {#if draw?.next}
    <div>height <strong>{draw.next.height}</strong> ·
      {#if draw.next.jackpot}<span class="pill warn">JACKPOT</span>{/if}
      {#if draw.next.pot_event}<span class="pill ok">POT · tier {draw.next.tier}</span>{/if}
      {#if !draw.next.jackpot && !draw.next.pot_event}<span class="pill">normal</span>{/if}
    </div>
    <div class="muted" style="margin-top:8px">digest {draw.next.digest}</div>
    <div class="muted" style="margin-top:4px">
      the ticket is SHA256D(parent hash ‖ height): fixed the moment the parent lands — the next
      height's class is exact; later heights are unknown until their parents exist (~1/100 each).
    </div>
  {:else}
    <div class="muted">node offline</div>
  {/if}
</div>

<h2>Recent game events</h2>
<table>
  <thead><tr><th>height</th><th>finder</th><th>jackpot</th><th>pot splash</th></tr></thead>
  <tbody>
    {#each blocks as b}
      <tr>
        <td>{b.height}</td>
        <td class="muted">{api.short(b.finder_name ?? b.finder, 10)}</td>
        <td>{api.coin(b.bonus)}</td>
        <td>{api.coin(b.pot)}</td>
      </tr>
    {/each}
    {#if blocks.length === 0}<tr><td colspan="4" class="muted">no jackpot/pot events in the pool's recent window</td></tr>{/if}
  </tbody>
</table>

<h2>Network</h2>
<div class="grid">
  <div class="card"><div class="k">network hashrate</div><div class="v">{mining.networkhashps ?? "–"} H/s</div></div>
  <div class="card"><div class="k">chain height</div><div class="v">{mining.blocks ?? "–"}</div></div>
  <div class="card"><div class="k">pool 10m</div><div class="v">{pool?.pool?.hashrate10m ? Math.round(pool.pool.hashrate10m) + " H/s" : "–"}</div></div>
  <div class="card"><div class="k">emission</div><div class="v" style="font-size:14px">ramp 0→16 · 16/8/4/2 · tail 1/block</div></div>
</div>
{#if err}<div class="err">{err}</div>{/if}
