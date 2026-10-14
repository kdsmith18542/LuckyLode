<script lang="ts">
  import Overview from "./views/Overview.svelte";
  import Send from "./views/Send.svelte";
  import Receive from "./views/Receive.svelte";
  import Miner from "./views/Miner.svelte";
  import Luckynodes from "./views/Luckynodes.svelte";
  import Game from "./views/Game.svelte";
  import Settings from "./views/Settings.svelte";
  import * as api from "./lib/api";

  let view = $state("overview");
  let status = $state<any>(null);
  let draw = $state<any>(null);
  let svc = $state<any>(null);
  let err = $state("");

  const nav: [string, string][] = [
    ["overview", "Overview"],
    ["send", "Send"],
    ["receive", "Receive"],
    ["miner", "Miner"],
    ["luckynodes", "Luckynodes"],
    ["game", "Game"],
    ["settings", "Settings"],
  ];

  async function refresh() {
    try {
      status = await api.nodeStatus();
      draw = await api.nextDraw();
      svc = await api.serviceStatus();
      err = "";
    } catch (e) {
      err = String(e);
    }
  }

  $effect(() => {
    refresh();
    const t = setInterval(refresh, 10000);
    return () => clearInterval(t);
  });

  const height = $derived(status?.blocks ?? "–");
  const chain = $derived(status?.chain ?? "–");
  const progress = $derived(
    typeof status?.verificationprogress === "number"
      ? `${(status.verificationprogress * 100).toFixed(1)}%`
      : "–",
  );
  const next = $derived(draw?.next ?? null);
  const running: string[] = $derived(svc?.running ?? []);
</script>

<div class="shell">
  <aside class="sidebar">
    <div class="brand">Lucky Lode<small>wallet · testnet</small></div>
    {#each nav as [id, label]}
      <button class="nav" class:active={view === id} onclick={() => (view = id)}>{label}</button>
    {/each}
  </aside>
  <main class="main">
    <div class="topbar">
      <span class="pill"><span class="dot" class:off={!!err}></span>{chain}</span>
      <span class="pill">height {height}</span>
      <span class="pill">sync {progress}</span>
      <span class="pill" class:ok={running.length > 0} class:warn={running.length === 0}>
        luckynode service {running.length > 0 ? `running (${running.length})` : "off"}
      </span>
      {#if err}
        <span class="pill bad" title={err}>
          {err.includes("401") || err.toLowerCase().includes("rpc login")
            ? "auth failed → Settings"
            : "node offline"}
        </span>
      {/if}
    </div>

    {#if next?.jackpot}
      <div class="banner">⚡ NEXT BLOCK ({next.height}) IS A JACKPOT BLOCK — bonus min(5R, reserve) if mined now</div>
    {:else if next?.pot_event}
      <div class="banner pot">💧 NEXT BLOCK ({next.height}) IS A POT EVENT — tier {next.tier} splash</div>
    {/if}

    {#if view === "overview"}<Overview />{:else if view === "send"}<Send />{:else if view === "receive"}<Receive />{:else if view === "miner"}<Miner />{:else if view === "luckynodes"}<Luckynodes />{:else if view === "game"}<Game />{:else if view === "settings"}<Settings />{/if}
  </main>
</div>
