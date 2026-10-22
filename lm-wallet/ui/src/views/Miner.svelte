<script lang="ts">
  import * as api from "../lib/api";
  let cfg = $state<any>({ pool_stratum: "", mining_worker: "" });
  let mode = $state<"pool" | "solo">("pool");
  let device = $state("auto");
  let threads = $state(4);
  let duty = $state(100);
  let maxTemp = $state(80);
  let gpus = $state<any[]>([]);
  let gpuSel = $state("0:0");
  let gpuBatch = $state(512);
  let maxPower = $state(0);
  let st = $state<any>(null);
  let log = $state<string[]>([]);
  let err = $state("");
  let msgSolo = $state("");
  let busy = $state(false);

  $effect(() => {
    api
      .getConfig()
      .then((c: any) => (cfg = c))
      .catch((e) => (err = String(e)));
    api
      .minerGpus()
      .then((g: any) => {
        gpus = g as any[];
        if (gpus.length > 0) gpuSel = `${gpus[0].platform}:${gpus[0].device}`;
      })
      .catch(() => (gpus = []));
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
  const useGpu = $derived(device !== "cpu");

  async function start() {
    err = "";
    busy = true;
    try {
      const [gp, gd] = gpuSel.split(":").map((x) => Number(x) || 0);
      const knobs = {
        device,
        threads: Number(threads) || 1,
        duty: Number(duty) || 100,
        max_temp: Number(maxTemp) || 80,
        gpu_platform: gp,
        gpu_device: gd,
        gpu_batch: Number(gpuBatch) || 512,
        max_power: Number(maxPower) || 0,
      };
      if (mode === "solo") {
        await api.minerSoloStart(knobs);
      } else {
        await api.setMiningConfig(cfg.pool_stratum, cfg.mining_worker.trim());
        await api.minerStart({
          pool: cfg.pool_stratum,
          worker: cfg.mining_worker.trim(),
          ...knobs,
        });
      }
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  async function pointPayouts() {
    err = "";
    msgSolo = "";
    try {
      const r: any = await api.minerSoloPointPayouts();
      msgSolo = `node payouts → ${r.mineraddress} (restart the node to apply)`;
    } catch (e) {
      err = String(e);
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
    pacing and a temperature guard.
  </p>
  <div class="row" style="gap:16px;margin-bottom:8px">
    <label class="row" style="align-items:center;gap:6px">
      <input type="radio" name="mode" value="pool" bind:group={mode} disabled={running} style="width:auto" />
      <span>pool <span class="muted">(any node + stratum pool; payouts to the worker address)</span></span>
    </label>
    <label class="row" style="align-items:center;gap:6px">
      <input type="radio" name="mode" value="solo" bind:group={mode} disabled={running} style="width:auto" />
      <span>solo <span class="muted">(your node only; claim-aware templates, rewards straight to your wallet)</span></span>
    </label>
  </div>
  {#if mode === "pool"}
    <label for="st">stratum endpoint (host:port)</label>
    <input id="st" bind:value={cfg.pool_stratum} spellcheck="false" disabled={running} />
    <label for="wk">worker name (is the payout address — the pool pays here; blank = your wallet)</label>
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
  {:else}
  <p class="muted">
    Solo mines getblocktemplate jobs from <strong>Node on this computer</strong> with the
    same claim-aware coinbases the internal miner builds (jackpot + pot + prize) — no pool
    in the loop. Rewards land wherever the node's coinbase pays: the miner refuses to
    start unless that is one of your wallet addresses.
  </p>
  <div class="row" style="margin-top:4px">
    <button class="btn" onclick={pointPayouts} disabled={running}>point node payouts at my wallet</button>
  </div>
  {#if msgSolo}<div class="muted" style="margin-top:6px">{msgSolo}</div>{/if}
  {/if}
  <label for="dv">device</label>
  <select id="dv" bind:value={device} disabled={running}>
    <option value="auto">auto (GPU when present, else CPU)</option>
    <option value="cpu">cpu</option>
    <option value="opencl">opencl (GPU)</option>
  </select>
  <div class="row" style="margin-top:4px">
    <div>
      <label for="th">threads (cpu)</label>
      <input id="th" type="number" min="1" max="64" bind:value={threads} disabled={running || useGpu} style="width:90px" />
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
  {#if useGpu}
    <label for="gpu">GPU</label>
    {#if gpus.length > 0}
      <select id="gpu" bind:value={gpuSel} disabled={running}>
        {#each gpus as g}
          <option value={`${g.platform}:${g.device}`}>{g.name} (platform {g.platform}, device {g.device})</option>
        {/each}
      </select>
    {:else}
      <div class="muted">no OpenCL devices found — install the vendor driver (the miner falls back to CPU with "auto")</div>
    {/if}
    <div class="row" style="margin-top:4px">
      <div>
        <label for="gb" title="search batch size (work units per GPU pass), not VRAM size">gpu work batch</label>
        <input id="gb" type="number" min="1" max="8192" bind:value={gpuBatch} disabled={running} style="width:110px" />
      </div>
      <div>
        <label for="mp">power limit W (0 = off)</label>
        <input id="mp" type="number" min="0" max="1000" bind:value={maxPower} disabled={running} style="width:110px" />
      </div>
    </div>
  {/if}
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
    {#if !String(st.pool ?? "").startsWith("solo")}
    <div class="card" title="resubmitted shares the pool already counted — not extra work"><div class="k">duplicates</div><div class="v">{st.duplicates}</div></div>
    {/if}
    <div class="card"><div class="k">hashes</div><div class="v">{st.hashes}</div></div>
    <div class="card"><div class="k">uptime</div><div class="v">{uptime(st.uptime_secs)}</div></div>
    {#if st.temp_c > 0}
      <div class="card"><div class="k">temp</div><div class="v">{st.temp_c}°C</div></div>
    {/if}
    {#if st.power_w}
      <div class="card"><div class="k">power</div><div class="v">{st.power_w} W</div></div>
    {/if}
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
