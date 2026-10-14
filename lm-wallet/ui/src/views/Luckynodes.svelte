<script lang="ts">
  import * as api from "../lib/api";
  let data = $state<any>(null);
  let svc = $state<any>(null);
  let addr = $state("");
  let msg = $state("");
  let err = $state("");

  async function load() {
    try {
      data = await api.luckynodes();
      svc = await api.serviceStatus();
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

  async function register() {
    msg = "";
    err = "";
    try {
      const r: any = await api.luckynodeRegister(addr.trim());
      msg = `registered ${api.short(r.id, 16)} — key stored at ${r.key_path}`;
      await load();
    } catch (e) {
      err = String(e);
    }
  }
  async function start(n: any) {
    try {
      await api.serviceStart(n.id, n.address);
      await load();
    } catch (e) {
      err = String(e);
    }
  }
  async function stop(n: any) {
    try {
      await api.serviceStop(n.id);
      await load();
    } catch (e) {
      err = String(e);
    }
  }

  const nodes = $derived<any[]>(data?.nodes ?? []);
  const draws = $derived<any[]>(data?.recent_draws ?? []);
  const running: string[] = $derived(svc?.running ?? []);
  const bondAmount = $derived(data?.config?.bond ?? 0);
  const days = (s: unknown) => Math.floor((Number(s) || 0) / 86400);
  const weight = (w: unknown) => ((Number(w) || 1000) / 1000).toFixed(1) + "×";
</script>

<h2>Luckynodes</h2>
<div class="grid">
  <div class="card"><div class="k">registered</div><div class="v">{nodes.length}</div></div>
  <div class="card"><div class="k">active (heartbeat)</div><div class="v">{data?.active?.length ?? 0}</div></div>
  <div class="card"><div class="k">bond required</div><div class="v gold">{api.coin(bondAmount)}</div></div>
  <div class="card"><div class="k">draw epoch</div><div class="v">{data?.config?.epoch ?? "–"} blocks</div></div>
</div>

<table style="margin-top:14px">
  <thead>
    <tr><th>node</th><th>payout address</th><th>bond</th><th>active</th><th>tenure</th><th>weight</th><th>last heartbeat</th><th>service</th></tr>
  </thead>
  <tbody>
    {#each nodes as n}
      <tr>
        <td>{api.short(n.id, 10)}</td>
        <td class="muted">{api.short(n.address, 10)}</td>
        <td><span class="pill" class:ok={n.bond_ok} class:bad={!n.bond_ok}>{n.bond_ok ? "bonded" : "under"}</span></td>
        <td><span class="pill" class:ok={data.active?.includes(n.id)} class:off={!data.active?.includes(n.id)}>{data.active?.includes(n.id) ? "yes" : "idle"}</span></td>
        <td>{days(n.tenure_secs)}d</td>
        <td class="muted">{weight(n.weight_millis)}</td>
        <td class="muted">{api.ago(n.last_heartbeat)}</td>
        <td>
          {#if running.includes(n.id)}
            <button class="btn danger" onclick={() => stop(n)}>Stop</button>
          {:else}
            <button class="btn" onclick={() => start(n)}>Start</button>
          {/if}
        </td>
      </tr>
    {/each}
    {#if nodes.length === 0}<tr><td colspan="8" class="muted">no luckynodes registered on this pool yet</td></tr>{/if}
  </tbody>
</table>

<h2>Register a luckynode</h2>
<div class="card" style="max-width:640px">
  <label for="addr">payout address (holds the bond)</label>
  <input id="addr" bind:value={addr} placeholder="L… / m… / n…" spellcheck="false" />
  <div class="row" style="margin-top:12px">
    <button class="btn primary" disabled={!addr} onclick={register}>Generate key + register</button>
    <span class="muted">ed25519 identity stored 0600 in the app dir; the wallet can run the service for you</span>
  </div>
  {#if msg}<div class="muted" style="margin-top:10px">{msg}</div>{/if}
</div>

<h2>Recent draws</h2>
<table>
  <thead><tr><th>epoch</th><th>height</th><th>winner</th><th>budget</th><th>txid</th></tr></thead>
  <tbody>
    {#each draws as d}
      <tr>
        <td>{d.epoch}</td>
        <td>{d.height}</td>
        <td>{api.short(d.winner, 10)}</td>
        <td>{api.coin(d.budget)}</td>
        <td class="muted">{api.short(d.txid, 12)}</td>
      </tr>
    {/each}
    {#if draws.length === 0}<tr><td colspan="5" class="muted">no draws yet — the next epoch boundary fires one</td></tr>{/if}
  </tbody>
</table>
{#if err}<div class="err">{err}</div>{/if}
