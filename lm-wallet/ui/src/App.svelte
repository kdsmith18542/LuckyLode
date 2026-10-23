<script lang="ts">
  import Overview from "./views/Overview.svelte";
  import Send from "./views/Send.svelte";
  import Receive from "./views/Receive.svelte";
  import Miner from "./views/Miner.svelte";
  import Luckynodes from "./views/Luckynodes.svelte";
  import Rewards from "./views/Rewards.svelte";
  import Settings from "./views/Settings.svelte";
  import * as api from "./lib/api";
  import { getTheme, setTheme } from "./lib/theme";
  import { pollEventNotifications } from "./lib/events";

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
  let setupErr = $state("");
  let busy = $state(false);
  let pw = $state("");
  let pw2 = $state("");
  // First-run form state: create and restore are separate tabs with
  // separate password fields (sharing one variable used to mirror
  // typing across both forms).
  let firstTab = $state<"create" | "restore">("create");
  let cpw = $state("");
  let cpw2 = $state("");
  let rPhrase = $state("");
  let rpw = $state("");
  let rpw2 = $state("");
  // Lock-screen recovery uses its own fields so typing a recovery phrase
  // never mutates the unlock/create password state.
  let showRecover = $state(false);
  let recPhrase = $state("");
  let recPw = $state("");
  let recPw2 = $state("");

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
      const r: any = await api.walletCreate(cpw);
      phrase = r.phrase;
      cpw = "";
      cpw2 = "";
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
      await api.walletRestore(rPhrase.trim(), rpw);
      rPhrase = "";
      rpw = "";
      rpw2 = "";
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
  async function recoverFromLock() {
    // Password lost: re-seal the vault from the backup phrase. This
    // REPLACES the locked wallet on this machine — the old password
    // stops working and anything not covered by these words is gone
    // from the app, so the form says so and requires the repeat.
    busy = true;
    setupErr = "";
    try {
      await api.walletRestore(recPhrase.trim(), recPw);
      recPhrase = "";
      recPw = "";
      recPw2 = "";
      showRecover = false;
      pw = "";
      hasWallet = true;
      locked = false;
      legacy = false;
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
    ["rewards", "Rewards"],
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
    // Auto-lock flip: the backend wipes the
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
    // Event notifications run on their own slower tick, independent of
    // which tab is mounted (the views only load display data).
    pollEventNotifications();
    const e = setInterval(pollEventNotifications, 30000);
    return () => {
      clearInterval(t);
      clearInterval(e);
    };
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
          <div class="row" style="margin-bottom:12px">
            <button class="btn" class:primary={firstTab === "create"} disabled={busy} onclick={() => { firstTab = "create"; setupErr = ""; }}>New wallet</button>
            <button class="btn" class:primary={firstTab === "restore"} disabled={busy} onclick={() => { firstTab = "restore"; setupErr = ""; }}>Restore from phrase</button>
          </div>
          {#if firstTab === "create"}
            <label for="npw">spending password (min 8 characters, encrypts the seed)</label>
            <input id="npw" type="password" bind:value={cpw} spellcheck="false" />
            <label for="npw2">repeat password</label>
            <input id="npw2" type="password" bind:value={cpw2} spellcheck="false" />
            {#if cpw && cpw2 && cpw !== cpw2}<div class="err" role="alert">passwords do not match</div>{/if}
            <div class="row" style="margin-top:10px">
              <button class="btn primary" disabled={busy || cpw.length < 8 || cpw !== cpw2} onclick={createWallet}>{busy ? "creating…" : "Create new wallet"}</button>
            </div>
          {:else}
            <label for="rest">12-word backup phrase</label>
            <input id="rest" bind:value={rPhrase} placeholder="twelve words separated by spaces" spellcheck="false" />
            <label for="rpw">spending password for this install (min 8 characters)</label>
            <input id="rpw" type="password" bind:value={rpw} spellcheck="false" />
            <label for="rpw2">repeat password</label>
            <input id="rpw2" type="password" bind:value={rpw2} spellcheck="false" />
            {#if rpw && rpw2 && rpw !== rpw2}<div class="err" role="alert">passwords do not match</div>{/if}
            <div class="row" style="margin-top:10px">
              <button class="btn primary" disabled={busy || !rPhrase.trim() || rpw.length < 8 || rpw !== rpw2} onclick={restoreWallet}>{busy ? "restoring…" : "Restore"}</button>
            </div>
          {/if}
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
        {#if setupErr}<div class="err" role="alert" style="margin-top:10px">{setupErr}</div>{/if}
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
        {#if setupErr}<div class="err" role="alert" style="margin-top:10px">{setupErr}</div>{/if}
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
        {#if setupErr}<div class="err" role="alert" style="margin-top:10px">{setupErr}</div>{/if}
        <div style="margin-top:12px">
          <button class="btn" disabled={busy} onclick={() => { showRecover = !showRecover; setupErr = ""; }}>
            {showRecover ? "Back to unlock" : "Forgot password?"}
          </button>
        </div>
      </div>
      {#if showRecover}
        <div class="card" style="margin-top:12px;text-align:left">
          <b>Recover from backup phrase</b>
          <p class="muted">
            This <b>replaces</b> the locked wallet on this machine with
            whatever these 12 words describe. The old password stops
            working; anything not covered by these words disappears from
            the app. Type carefully — a mistyped phrase fails the
            checksum and restores nothing.
          </p>
          <label for="recphrase">12-word backup phrase</label>
          <input id="recphrase" bind:value={recPhrase} placeholder="twelve words separated by spaces" spellcheck="false" />
          <label for="recpw">new spending password (min 8 characters)</label>
          <input id="recpw" type="password" bind:value={recPw} spellcheck="false" />
          <label for="recpw2">repeat new password</label>
          <input id="recpw2" type="password" bind:value={recPw2} spellcheck="false" />
          {#if recPw && recPw2 && recPw !== recPw2}<div class="err" role="alert">passwords do not match</div>{/if}
          <div class="row" style="margin-top:10px">
            <button class="btn primary" disabled={busy || !recPhrase.trim() || recPw.length < 8 || recPw !== recPw2} onclick={recoverFromLock}>
              Replace wallet from phrase
            </button>
          </div>
        </div>
      {/if}
    </main>
  </div>
{:else}
<div class="shell">
  <aside class="sidebar">
    <div class="brand">Lucky Lode<small>wallet · {chain === "test" ? "testnet" : chain === "main" ? "mainnet" : chain}</small></div>
    {#each nav as [id, label]}
      <button class="nav" class:active={view === id} aria-current={view === id ? "page" : undefined} onclick={() => (view = id)}>{label}</button>
    {/each}
  </aside>
  <main class="main">
    <div class="topbar">
      <span class="pill"><span class="dot" class:off={!!err} role="img" aria-label={err ? "node unreachable" : "node connected"}></span>{chain}</span>
      <span class="pill">height {height}</span>
      <span class="pill">chain sync {progress}</span>
      <span class="pill" class:ok={running.length > 0} class:warn={running.length === 0}>
        local luckynode service {running.length > 0 ? `running (${running.length})` : "off"}
      </span>
      {#if err}
        <span class="pill bad" role="alert" aria-live="polite" title={err}>
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
      <div class="banner pot">💧 NEXT BLOCK ({next.height}) IS A POT EVENT — tier {next.tier} ({next.pot_pct}% of the pot) splash</div>
    {/if}

    {#if view === "overview"}<Overview />{:else if view === "send"}<Send />{:else if view === "receive"}<Receive />{:else if view === "miner"}<Miner />{:else if view === "luckynodes"}<Luckynodes />{:else if view === "rewards"}<Rewards />{:else if view === "settings"}<Settings />{/if}
  </main>
</div>
{/if}
