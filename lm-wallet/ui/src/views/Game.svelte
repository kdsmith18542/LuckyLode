<script lang="ts">
  import * as api from "../lib/api";
  let info = $state<any>(null);
  let pool = $state<any>(null);
  let draw = $state<any>(null);
  let em = $state<any>(null);
  let err = $state("");

  async function load() {
    try {
      info = await api.gameInfo();
      pool = await api.poolStats();
      draw = await api.nextDraw();
      em = await api.emission();
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
  const events = $derived<any[]>(
    (pool?.last_blocks ?? []).filter(
      (b: any) => b.bonus > 0 || b.pot > 0 || b.league_overflow > 0,
    ),
  );
  const draws = $derived<any[]>(pool?.luckynode?.recent_draws ?? []);
  const league = $derived<number | null>(
    typeof pool?.pool?.league_pool === "number" ? pool.pool.league_pool : null,
  );
  const droughtLeft = $derived(
    typeof jp.drought === "number" && typeof jp.potdrought === "number"
      ? Math.max(0, jp.potdrought - jp.drought)
      : null,
  );

  // Luck chart: stacked bars per recent block — league inflow (dim),
  // pot splash (green), jackpot bonus (gold), scaled to the window max.
  const chart = $derived.by(() => {
    const rows = (pool?.last_blocks ?? []).slice(0, 40).reverse() as any[];
    const vals = rows.map((b: any) => ({
      h: b.height ?? 0,
      bonus: b.bonus ?? 0,
      pot: b.pot ?? 0,
      league: b.league_overflow ?? 0,
    }));
    const max = Math.max(1, ...vals.map((v) => v.bonus + v.pot + v.league));
    return { vals, max, w: Math.max(vals.length, 1) * 12 };
  });

  function txLabel(t: unknown): string {
    const s = String(t ?? "");
    if (s.length === 64 && /^[0-9a-f]+$/i.test(s)) return api.short(s, 10);
    return s.length > 28 ? s.slice(0, 28) + "…" : s || "–";
  }
</script>

<h2>Game state</h2>
<div class="grid">
  <div class="card"><div class="k">jackpot reserve</div><div class="v gold">{api.coin(jp.reserve)}</div></div>
  <div class="card"><div class="k">lucky pot</div><div class="v">{api.coin(jp.pot)}</div></div>
  <div class="card">
    <div class="k">league pool</div>
    <div class="v">{league !== null ? api.coin(league) : "–"}</div>
  </div>
  <div class="card">
    <div class="k">pot pity in</div>
    <div class="v">{droughtLeft !== null ? `${droughtLeft} blocks` : "–"}</div>
  </div>
  <div class="card"><div class="k">difficulty</div><div class="v">{mining.difficulty ?? "–"}</div></div>
</div>

<h2>Emission (docs/04 §4.1)</h2>
<div class="card" style="max-width:720px">
  {#if em}
    <div class="row" style="justify-content:space-between">
      <div><span class="muted">phase</span> <strong>{em.phase}</strong> <span class="muted">(era {em.era})</span></div>
      <div>
        <span class="muted">reward</span> <strong>{api.coin(em.reward)}</strong>
        <span class="muted">gross · {api.coin(em.net)} after the 5% levy</span>
      </div>
    </div>
    <div style="margin-top:10px;background:var(--active);border-radius:6px;height:8px;overflow:hidden">
      <div style="height:8px;background:var(--gold);width:{Math.min(100, (em.progress ?? 0) * 100).toFixed(1)}%"></div>
    </div>
    <div class="muted" style="margin-top:6px">
      {#if em.next_milestone}
        {em.next_milestone.label} at height {em.next_milestone.height}
        ({Math.max(0, em.next_milestone.height - em.height)} blocks away)
      {:else}
        tail forever — no further halvings
      {/if}
    </div>
  {:else}
    <div class="muted">node offline</div>
  {/if}
</div>

<h2>Luck chart (recent blocks)</h2>
<div class="card">
  {#if chart.vals.length > 0}
    <svg
      viewBox={`0 0 ${chart.w} 90`}
      width="100%"
      height="120"
      preserveAspectRatio="none"
      role="img"
      aria-label="game events per block"
    >
      {#each chart.vals as v, i}
        {@const bh = (v.bonus / chart.max) * 80}
        {@const ph = (v.pot / chart.max) * 80}
        {@const lh = (v.league / chart.max) * 80}
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
      <span style="color:var(--green);opacity:.5">■</span> league inflow
    </div>
  {:else}
    <div class="muted">no blocks in the pool's window</div>
  {/if}
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

<h2>Luckynode Draws</h2>
<table>
  <thead><tr><th>epoch</th><th>height</th><th>winner</th><th>budget</th><th>cosigns</th><th>txid</th></tr></thead>
  <tbody>
    {#each draws as d}
      <tr>
        <td>{d.epoch}</td>
        <td>{d.height}</td>
        <td class="muted">{api.short(d.winner, 12)}</td>
        <td>{api.coin(d.budget)}</td>
        <td>{d.cosigns ?? 0}</td>
        <td class="muted" title={String(d.txid ?? "")}>{txLabel(d.txid)}</td>
      </tr>
    {/each}
    {#if draws.length === 0}<tr><td colspan="6" class="muted">no draws in the pool's window</td></tr>{/if}
  </tbody>
</table>

<h2>Recent game events</h2>
<table>
  <thead><tr><th>height</th><th>finder</th><th>jackpot</th><th>pot splash</th><th>league inflow</th></tr></thead>
  <tbody>
    {#each events as b}
      <tr>
        <td>{b.height}</td>
        <td class="muted">{api.short(b.finder_name ?? b.finder, 10)}</td>
        <td>{api.coin(b.bonus)}</td>
        <td>{api.coin(b.pot)}</td>
        <td>{api.coin(b.league_overflow)}</td>
      </tr>
    {/each}
    {#if events.length === 0}<tr><td colspan="5" class="muted">no jackpot/pot/league events in the pool's recent window</td></tr>{/if}
  </tbody>
</table>

<h2>Network</h2>
<div class="grid">
  <div class="card"><div class="k">network hashrate</div><div class="v">{mining.networkhashps ?? "–"} H/s</div></div>
  <div class="card"><div class="k">chain height</div><div class="v">{mining.blocks ?? "–"}</div></div>
  <div class="card"><div class="k">pool 10m</div><div class="v">{pool?.pool?.hashrate10m ? Math.round(pool.pool.hashrate10m) + " H/s" : "–"}</div></div>
  <div class="card"><div class="k">emission plan</div><div class="v" style="font-size:14px">ramp 0→16 · 16/8/4/2 · tail 1/block</div></div>
</div>
{#if err}<div class="err">{err}</div>{/if}
