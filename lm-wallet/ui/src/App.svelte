<script lang="ts">
  import Overview from "./views/Overview.svelte";
  import Send from "./views/Send.svelte";
  import Receive from "./views/Receive.svelte";
  import Miner from "./views/Miner.svelte";
  import Luckynodes from "./views/Luckynodes.svelte";
  import Game from "./views/Game.svelte";
  import Settings from "./views/Settings.svelte";
  import * as api from "./lib/api";
  import { getTheme, setTheme } from "./lib/theme";

  let view = $state("overview");
  let status = $state<any>(null);
  let draw = $state<any>(null);
  let svc = $state<any>(null);
  let err = $state("");
  let theme = $state<"dark" | "light">(getTheme());

  // ---- first-run gate: the app needs its own wallet to do anything -----
  let hasWallet = $state<boolean | null>(null);
  let locked = $state(false);
  let legacy = $state(false);
  let phrase = $state<string | null>(null);
  let confirmBackup = $state(false);
  let restoreInput = $state("");
  let setupErr = $state("");
  let busy = $state(false);
  let pw = $state("");
  let pw2 = $state("");

  async function checkWallet() {
    try {
      const r: any = await api.walletStatus();
      hasWallet = !!r.has_wallet;
      locked = !!r.locked;
      legacy = !!r.legacy;
    } catch (e) {
      setupErr = String(e);
      hasWallet = true; // never lock the UI out on an unexpected error
      locked = false;
    }
  }
  $effect(() => {
    checkWallet();
  });

  async function createWallet() {
    busy = true;
    setupErr = "";
    try {
      const r: any = await api.walletCreate(pw);
      phrase = r.phrase;
      pw = "";
      pw2 = "";
      // Stay on this screen until the phrase is acknowledged (Continue):
      // re-checking here used to flip straight into the wallet and hide
      // the seed before it could be written down (2026-10-16).
    } catch (e) {
      setupErr = String(e);
    } finally {
      busy = false;
    }
  }
  async function restoreWallet() {
    busy = true;
    setupErr = "";
    try {
      await api.walletRestore(restoreInput.trim(), pw);
      restoreInput = "";
      pw = "";
      pw2 = "";
      hasWallet = true;
      locked = false;
      legacy = false;
    } catch (e) {
      setupErr = String(e);
    } finally {
      busy = false;
    }
  }
  async function unlockWallet() {
    busy = true;
    setupErr = "";
    try {
      await api.walletUnlock(pw);
      pw = "";
      await checkWallet();
    } catch (e) {
      setupErr = String(e);
    } finally {
      busy = false;
    }
  }
  async function migrateWallet() {
    busy = true;
    setupErr = "";
    try {
      await api.walletMigrate(pw);
      pw = "";
      pw2 = "";
      await checkWallet();
    } catch (e) {
      setupErr = String(e);
    } finally {
      busy = false;
    }
  }
  async function finishSetup() {
    phrase = null;
    await checkWallet();
  }

  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    setTheme(theme);
  }

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
      api.setChain((status as any)?.chain);
      draw = await api.nextDraw();
      svc = await api.serviceStatus();
      err = "";
    } catch (e) {
      err = String(e);
    }
    // Auto-lock flip (review fix 2026-10-18): the backend wipes the
    // session after 15 min idle, but the UI used to stay open showing
    // balances — the user found out via red errors. Re-check the lock
    // on every tick and drop straight to the lock screen.
    try {
      const r: any = await api.walletStatus();
      if (!!r.locked && !locked) {
        locked = true;
        pw = "";
      }
    } catch {
      // node/status hiccups must not lock anyone out
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

{#if hasWallet === false}
  <div class="shell" style="grid-template-columns:1fr">
    <main class="main" style="max-width:640px;margin:8vh auto 0">
      <div class="brand" style="font-size:22px">Lucky Lode<small>self-custody wallet · testnet</small></div>
      <div class="card" style="margin-top:16px">
        {#if phrase === null}
          <p class="muted">
            This app holds its own keys on this machine (12-word seed, BIP44 addresses).
            The node is just a data source and broadcast relay. Set a spending password —
            you'll enter it every time the app opens.
          </p>
          <label for="npw">spending password (min 8 characters, encrypts the seed)</label>
          <input id="npw" type="password" bind:value={pw} spellcheck="false" />
          <label for="npw2">repeat password</label>
          <input id="npw2" type="password" bind:value={pw2} spellcheck="false" />
          <div class="row" style="margin-top:10px">
            <button class="btn primary" disabled={busy || pw.length < 8 || pw !== pw2} onclick={createWallet}>Create new wallet</button>
          </div>
          <label for="rest" style="margin-top:14px">or restore from backup phrase</label>
          <input id="rest" bind:value={restoreInput} placeholder="twelve words separated by spaces" spellcheck="false" />
          <label for="rpw">spending password for this install</label>
          <input id="rpw" type="password" bind:value={pw} spellcheck="false" />
          <div class="row">
            <button class="btn" disabled={busy || !restoreInput.trim() || pw.length < 8} onclick={restoreWallet}>Restore</button>
          </div>
        {:else}
          <b>Write these 12 words down — they are the only way to recover your funds.</b>
          <div class="addr" style="word-break:break-word;margin:10px 0;font-size:15px">{phrase}</div>
          <label class="row" style="align-items:center;gap:8px">
            <input type="checkbox" bind:checked={confirmBackup} style="width:auto" />
            <span class="muted">I saved the phrase somewhere safe</span>
          </label>
          <div class="row" style="margin-top:10px">
            <button class="btn primary" disabled={!confirmBackup} onclick={finishSetup}>Continue</button>
          </div>
        {/if}
        {#if setupErr}<div class="err" style="margin-top:10px">{setupErr}</div>{/if}
      </div>
    </main>
  </div>
{:else if legacy}
  <div class="shell" style="grid-template-columns:1fr">
    <main class="main" style="max-width:640px;margin:8vh auto 0">
      <div class="brand" style="font-size:22px">Lucky Lode<small>wallet upgrade</small></div>
      <div class="card" style="margin-top:16px">
        <p class="muted">
          This install still keeps its seed in plaintext. Set a spending password to
          seal it into the encrypted vault — the plaintext is shredded afterwards.
        </p>
        <label for="mpw">new spending password (min 8 characters)</label>
        <input id="mpw" type="password" bind:value={pw} spellcheck="false" />
        <label for="mpw2">repeat password</label>
        <input id="mpw2" type="password" bind:value={pw2} spellcheck="false" />
        <div class="row" style="margin-top:10px">
          <button class="btn primary" disabled={busy || pw.length < 8 || pw !== pw2} onclick={migrateWallet}>Encrypt wallet</button>
        </div>
        {#if setupErr}<div class="err" style="margin-top:10px">{setupErr}</div>{/if}
      </div>
    </main>
  </div>
{:else if locked}
  <div class="shell" style="grid-template-columns:1fr">
    <main class="main" style="max-width:480px;margin:12vh auto 0;text-align:center">
      <div class="brand" style="font-size:22px">🔒 Lucky Lode<small>wallet locked</small></div>
      <div class="card" style="margin-top:16px">
        <label for="upw" style="text-align:left">spending password</label>
        <input id="upw" type="password" bind:value={pw} spellcheck="false" onkeydown={(e) => { if (e.key === "Enter") unlockWallet(); }} />
        <div class="row" style="margin-top:10px;justify-content:center">
          <button class="btn primary" disabled={busy || !pw} onclick={unlockWallet}>Unlock</button>
        </div>
        {#if setupErr}<div class="err" style="margin-top:10px">{setupErr}</div>{/if}
      </div>
    </main>
  </div>
{:else}
<div class="shell">
  <aside class="sidebar">
    <div class="brand">Lucky Lode<small>wallet · {chain === "test" ? "testnet" : chain === "main" ? "mainnet" : chain}</small></div>
    {#each nav as [id, label]}
      <button class="nav" class:active={view === id} onclick={() => (view = id)}>{label}</button>
    {/each}
  </aside>
  <main class="main">
    <div class="topbar">
      <span class="pill"><span class="dot" class:off={!!err}></span>{chain}</span>
      <span class="pill">height {height}</span>
      <span class="pill">chain sync {progress}</span>
      <span class="pill" class:ok={running.length > 0} class:warn={running.length === 0}>
        local luckynode service {running.length > 0 ? `running (${running.length})` : "off"}
      </span>
      {#if err}
        <span class="pill bad" title={err}>
          {err.includes("401") || err.toLowerCase().includes("rpc login")
            ? "auth failed → Settings"
            : status?.shared_node
              ? "shared node offline"
              : "no local node · use the shared node (Settings)"}
        </span>
      {/if}
      <button
        class="btn"
        style="margin-left:auto"
        title="switch theme"
        onclick={toggleTheme}
      >
        {theme === "dark" ? "☀ light" : "☾ dark"}
      </button>
    </div>

    {#if next?.jackpot}
      <div class="banner">⚡ NEXT BLOCK ({next.height}) IS A JACKPOT BLOCK — bonus up to 5× the block reward, capped by the reserve</div>
    {:else if next?.pot_event}
      <div class="banner pot">💧 NEXT BLOCK ({next.height}) IS A POT EVENT — tier {next.tier} ({next.tier}% of the pot) splash</div>
    {/if}

    {#if view === "overview"}<Overview />{:else if view === "send"}<Send />{:else if view === "receive"}<Receive />{:else if view === "miner"}<Miner />{:else if view === "luckynodes"}<Luckynodes />{:else if view === "game"}<Game />{:else if view === "settings"}<Settings />{/if}
  </main>
</div>
{/if}
