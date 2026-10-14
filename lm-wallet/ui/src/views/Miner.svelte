<script lang="ts">
  import * as api from "../lib/api";
  let cfg = $state<any>({ pool_stratum: "", mining_worker: "" });
  let device = $state("auto");
  let threads = $state(4);
  let duty = $state(100);
  let maxTemp = $state(80);
  let st = $state<any>(null);
  let log = $state<string[]>([]);
  let err = $state("");
  let busy = $state(false);

  $effect(() => {
    api
      .getConfig()
      .then((c: any) => (cfg = c))
      .catch((e) => (err = String(e)));
    const t = setInterval(async () => {
      try {
        st = await api.minerStatus();
        log = await api.minerLog();
      } catch {
        /* backend busy; next tick */
      }
    }, 2000);
    return () => clearInterval(t);
  });

  const running = $derived(!!st?.running);

  async function start() {
    err = "";
    busy = true;
    try {
      await api.setMiningConfig(cfg.pool_stratum, cfg.mining_worker.trim());
      await api.minerStart({
        pool: cfg.pool_stratum,
        worker: cfg.mining_worker.trim(),
        device,
        threads: Number(threads) || 1,
        duty: Number(duty) || 100,
        max_temp: Number(maxTemp) || 80,
      });
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  async function stop() {
    err = "";
    try {
      await api.minerStop();
    } catch (e) {
      err = String(e);
    }
  }

  async function freshAddress() {
    err = "";
    try {
      cfg.mining_worker = await api.newAddress();
    } catch (e) {
      err = String(e);
    }
  }

  const hs = (v: number) =>
    v >= 1e6
      ? (v / 1e6).toFixed(2) + " MH/s"
      : v >= 1e3
        ? (v / 1e3).toFixed(1) + " kH/s"
        : (v || 0).toFixed(0) + " H/s";
  const uptime = (s: number) => {
    s = s || 0;
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    return h > 0 ? `${h}h ${m}m` : `${m}m ${s % 60}s`;
  };
</script>

<h2>Miner</h2>
<div class="card" style="max-width:640px">
  <p class="muted" style="margin-top:0">
    Mine from the wallet — same engine as the reference <code>lm-miner</code>, with duty
    pacing and a temperature guard. Rewards go to the payout address below (your worker name).
  </p>
  <label for="st">stratum endpoint (host:port)</label>
  <input id="st" bind:value={cfg.pool_stratum} spellcheck="false" disabled={running} />
  <label for="wk">payout address (worker name — the pool pays here)</label>
  <div class="row">
    <input
      id="wk"
      bind:value={cfg.mining_worker}
      spellcheck="false"
      disabled={running}
      style="flex:1; min-width:280px"
    />
    <button class="btn" onclick={freshAddress} disabled={running}>new address</button>
  </div>
  <label for="dv">device</label>
  <select id="dv" bind:value={device} disabled={running}>
    <option value="auto">auto (GPU when present, else CPU)</option>
    <option value="cpu">cpu</option>
    <option value="opencl">opencl (GPU)</option>
  </select>
  <div class="row" style="margin-top:4px">
    <div>
      <label for="th">threads (cpu)</label>
      <input id="th" type="number" min="1" max="64" bind:value={threads} disabled={running} style="width:90px" />
    </div>
    <div>
      <label for="dt">duty %</label>
      <input id="dt" type="number" min="1" max="100" bind:value={duty} disabled={running} style="width:90px" />
    </div>
    <div>
      <label for="mt">max temp °C</label>
      <input id="mt" type="number" min="40" max="100" bind:value={maxTemp} disabled={running} style="width:90px" />
    </div>
  </div>
  <div class="row" style="margin-top:14px">
    {#if !running}
      <button class="btn primary" onclick={start} disabled={busy}>Start mining</button>
    {:else}
      <button class="btn danger" onclick={stop}>Stop mining</button>
    {/if}
    <span class="muted">{running ? "running" : "stopped"}</span>
  </div>
  {#if err}<div class="err">{err}</div>{/if}
</div>

{#if st && (running || st.hashes > 0)}
  <h2>Status</h2>
  <div class="grid">
    <div class="card"><div class="k">hashrate</div><div class="v">{hs(st.hashrate)}</div></div>
    <div class="card"><div class="k">accepted</div><div class="v">{st.accepted}</div></div>
    <div class="card"><div class="k">rejected</div><div class="v">{st.rejected}</div></div>
    <div class="card"><div class="k">duplicates</div><div class="v">{st.duplicates}</div></div>
    <div class="card"><div class="k">hashes</div><div class="v">{st.hashes}</div></div>
    <div class="card"><div class="k">uptime</div><div class="v">{uptime(st.uptime_secs)}</div></div>
  </div>
  <div class="row" style="margin-top:8px">
    <span class="muted">{st.device} · {st.pool} · worker {st.worker}</span>
  </div>
  {#if st.error}<div class="err">{st.error}</div>{/if}
{/if}

{#if log.length > 0}
  <h2>Log</h2>
  <div class="card log">{#each log as line}<div>{line}</div>{/each}</div>
{/if}

<style>
  .log {
    max-height: 220px;
    overflow-y: auto;
    font-size: 12px;
    color: var(--dim);
    white-space: pre-wrap;
  }
  .log div {
    padding: 1px 0;
  }
</style>
