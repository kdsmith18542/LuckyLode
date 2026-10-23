<script lang="ts">
  import * as api from "../lib/api";
  let info = $state<any>(null);
  let pool = $state<any>(null);
  let draw = $state<any>(null);
  let err = $state("");

  async function load() {
    // Per-source: a dead pool must not blank node-sourced cards and
    // vice versa.
    try {
      info = await api.gameInfo();
    } catch {
      info = null;
    }
    try {
      pool = await api.poolStats();
    } catch {
      pool = null;
    }
    try {
      draw = await api.nextDraw();
    } catch {
      draw = null;
    }
    err = "";
  }
  $effect(() => {
    load();
    const t = setInterval(load, 10000);
    return () => clearInterval(t);
  });

  const jp = $derived<any>(info?.jackpot ?? {});
  const mining = $derived<any>(info?.mining ?? {});
  const events = $derived<any[]>(
    (pool?.last_blocks ?? []).filter(
      (b: any) => b.bonus > 0 || b.pot > 0 || b.prize_inflow > 0,
    ),
  );
  const prize = $derived<number | null>(
    typeof jp.prize === "number" ? jp.prize : null,
  );
  const droughtLeft = $derived(
    typeof jp.drought === "number" && typeof jp.potdrought === "number"
      ? Math.max(0, jp.potdrought - jp.drought)
      : null,
  );

  // Reward chart: stacked bars per recent block — prize inflow (dim),
  // pot splash (green), jackpot bonus (gold), scaled to the window max.
  const chart = $derived.by(() => {
    const rows = (pool?.last_blocks ?? []).slice(0, 40).reverse() as any[];
    const vals = rows.map((b: any) => ({
      h: b.height ?? 0,
      bonus: b.bonus ?? 0,
      pot: b.pot ?? 0,
      prize: b.prize_inflow ?? 0,
    }));
    const max = Math.max(1, ...vals.map((v) => v.bonus + v.pot + v.prize));
    return { vals, max, w: Math.max(vals.length, 1) * 12 };
  });

</script>

<h2>Rewards</h2>
<div class="grid">
  <div class="card"><div class="k">jackpot reserve</div><div class="v gold">{api.coinU(jp.reserve)}</div></div>
  <div class="card"><div class="k">lucky pot</div><div class="v">{api.coinU(jp.pot)}</div></div>
  <div class="card">
    <div class="k">prize fund</div>
    <div class="v">{prize !== null ? api.coinU(prize) : "–"}</div>
  </div>
  <div class="card">
    <div class="k">forced pot splash in</div>
    <div class="v">{droughtLeft !== null ? `${droughtLeft} blocks` : "–"}</div>
  </div>
  <div class="card"><div class="k">difficulty</div><div class="v">{mining.difficulty ?? "–"}</div></div>
</div>

<h2>Reward events (recent blocks)</h2>
<div class="card">
  {#if chart.vals.length > 0}
    <svg
      viewBox={`0 0 ${chart.w} 90`}
      width="100%"
      height="120"
      preserveAspectRatio="none"
      role="img"
      aria-label="reward events per block"
    >
      {#each chart.vals as v, i}
        {@const bh = (v.bonus / chart.max) * 80}
        {@const ph = (v.pot / chart.max) * 80}
        {@const lh = (v.prize / chart.max) * 80}
        <rect x={i * 12} y={80 - lh} width="10" height={lh} fill="var(--green)" opacity="0.35" />
        <rect x={i * 12} y={80 - lh - ph} width="10" height={ph} fill="var(--green)" />
        <rect x={i * 12} y={80 - lh - ph - bh} width="10" height={bh} fill="var(--gold)" />
      {/each}
      <line x1="0" y1="80" x2={chart.w} y2="80" stroke="var(--line)" stroke-width="1" />
    </svg>
    <div class="muted" style="font-size:12px">
      heights {chart.vals[0].h}–{chart.vals[chart.vals.length - 1].h} ·
      <span style="color:var(--gold)">■</span> jackpot bonus
      <span style="color:var(--green)">■</span> pot splash
      <span style="color:var(--green);opacity:.5">■</span> prize inflow
    </div>
  {:else}
    <div class="muted">no blocks in the pool's window</div>
  {/if}
</div>

<h2>Next block draw</h2>
<div class="muted">the ticket is fixed before anyone grinds, so nothing can be withheld</div>
<div class="card" style="max-width:640px">
  {#if draw?.next}
    <div>height <strong>{draw.next.height}</strong> ·
      {#if draw.next.jackpot}<span class="pill warn">JACKPOT</span>{/if}
      {#if draw.next.pot_event}<span class="pill ok">POT · tier {draw.next.tier} ({draw.next.pot_pct}% of pot)</span>{/if}
      {#if !draw.next.jackpot && !draw.next.pot_event}<span class="pill">normal</span>{/if}
    </div>
    <div class="muted" style="margin-top:8px">draw ticket hash {draw.next.digest}</div>
    <div class="muted" style="margin-top:4px">
      the ticket is SHA256D(parent hash ‖ height): fixed the moment the parent lands — the next
      height's class is exact; later heights are unknown until their parents exist (jackpot ~1/100, pot ~1/1000 per block).
    </div>
  {:else}
    <div class="muted">node offline</div>
  {/if}
</div>

<h2>Recent reward events</h2>
<table>
  <thead><tr><th>height</th><th>finder</th><th>block reward</th><th>jackpot bonus</th><th>pot splash</th><th>prize inflow</th></tr></thead>
  <tbody>
    {#each events as b}
      <tr>
        <td>{b.height}</td>
        <td class="muted">{api.short(b.finder_name ?? b.finder, 10)}</td>
        <td>{api.coinU(b.value)}</td>
        <td>{api.coinU(b.bonus)}</td>
        <td>{api.coinU(b.pot)}</td>
        <td>{api.coinU(b.prize_inflow)}</td>
      </tr>
    {/each}
    {#if events.length === 0}<tr><td colspan="6" class="muted">no jackpot/pot/prize events in the pool's recent window</td></tr>{/if}
  </tbody>
</table>

{#if err}<div class="err">{err}</div>{/if}
