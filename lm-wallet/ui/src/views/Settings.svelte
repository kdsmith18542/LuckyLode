<script lang="ts">
  import * as api from "../lib/api";
  import { getTheme, setTheme } from "../lib/theme";
  let cfg = $state<any>({ rpc_url: "", rpc_user: "", rpc_pass: "", pool_url: "", node_bin: "", node_datadir: "", node_args: "-testnet" });
  let saved = $state("");
  let err = $state("");
  let test = $state("");
  let nodemsg = $state("");
  let nodestatus = $state<any>(null);
  let discover = $state<any>(null); // { candidates: string[], default_datadir: string }
  let advanced = $state(false);
  let theme = $state<"dark" | "light">(getTheme());

  // ---- self-custody wallet section -----------------------------------
  let ws = $state<any>(null);
  let phrase = $state("");
  let newPhrase = $state<string | null>(null);
  let confirmBackup = $state(false);
  let restoreInput = $state("");
  let rpw = $state("");
  let pw = $state("");
  let pw2 = $state("");
  let oldPw = $state("");
  let newPw = $state("");
  let newPw2 = $state("");
  let reveal = $state("");
  let revealWarn = $state(false);
  let revealLeft = $state(0); // seconds until the phrase auto-hides
  let releaseArm = $state(""); // two-step confirm for bond release
  let contacts = $state<any[]>([]);
  let contactRename = $state(""); // address being renamed
  let contactName = $state("");
  let wmsg = $state("");

  async function loadWallet() {
    try {
      ws = await api.walletStatus();
    } catch (e) {
      err = String(e);
    }
  }

  async function create() {
    wmsg = "";
    err = "";
    try {
      const r: any = await api.walletCreate(pw);
      newPhrase = r.phrase;
      confirmBackup = false;
      pw = "";
      pw2 = "";
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  async function restore(pass: string) {
    wmsg = "";
    err = "";
    try {
      await api.walletRestore(restoreInput.trim(), pass);
      restoreInput = "";
      rpw = "";
      wmsg = "wallet restored + rescanned";
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  async function unlock() {
    wmsg = "";
    err = "";
    try {
      await api.walletUnlock(pw);
      pw = "";
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  async function lockNow() {
    try {
      await api.walletLock();
      reveal = "";
      revealWarn = false;
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  async function migrate() {
    wmsg = "";
    err = "";
    try {
      await api.walletMigrate(pw);
      pw = "";
      pw2 = "";
      wmsg = "wallet encrypted — plaintext shredded";
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  async function changePw() {
    wmsg = "";
    err = "";
    try {
      await api.walletChangePassword(oldPw, newPw);
      oldPw = "";
      newPw = "";
      newPw2 = "";
      wmsg = "password changed";
    } catch (e) {
      err = String(e);
    }
  }

  async function release(addr: string) {
    if (releaseArm !== addr) {
      // Two-step: a stray click must not silently strip draw eligibility.
      releaseArm = addr;
      return;
    }
    releaseArm = "";
    wmsg = "";
    err = "";
    try {
      await api.unprotectAddress(addr);
      wmsg = "address released — spendable again (a luckynode using it loses draw eligibility)";
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  const protectedAddrs = $derived<string[]>(ws?.protected_addrs ?? []);

  const REVEAL_SECS = 60;

  async function doReveal() {
    wmsg = "";
    err = "";
    try {
      const r: any = await api.walletReveal();
      reveal = r.phrase;
      revealLeft = REVEAL_SECS;
    } catch (e) {
      err = String(e);
    }
  }

  // Auto-hide the phrase after REVEAL_SECS — an open backup phrase must
  // not sit on screen indefinitely.
  $effect(() => {
    if (revealLeft <= 0) return;
    const t = setInterval(() => {
      revealLeft -= 1;
      if (revealLeft <= 0) {
        reveal = "";
        revealWarn = false;
      }
    }, 1000);
    return () => clearInterval(t);
  });

  function hideReveal() {
    revealLeft = 0;
    reveal = "";
    revealWarn = false;
  }

  async function loadContacts() {
    try {
      contacts = ((await api.contactsList()) as any[]) ?? [];
    } catch {
      contacts = [];
    }
  }

  async function removeContact(addr: string) {
    try {
      await api.contactRemove(addr);
      wmsg = "contact removed";
      await loadContacts();
    } catch (e) {
      err = String(e);
    }
  }

  async function saveRename(addr: string) {
    const name = contactName.trim();
    contactRename = "";
    contactName = "";
    if (!name) return;
    try {
      await api.contactAdd(name, addr); // re-adding replaces the name
      wmsg = "contact renamed";
      await loadContacts();
    } catch (e) {
      err = String(e);
    }
  }

  async function rescan() {
    wmsg = "";
    err = "";
    try {
      const r: any = await api.walletRescan();
      wmsg = `rescanned: ${r.used_addresses} used address(es)`;
      await loadWallet();
    } catch (e) {
      err = String(e);
    }
  }

  function onTheme(e: Event) {
    const t = (e.currentTarget as HTMLSelectElement).value === "light" ? "light" : "dark";
    theme = t;
    setTheme(t);
  }

  $effect(() => {
    api
      .getConfig()
      .then((c: any) => (cfg = c))
      .catch((e) => (err = String(e)));
    loadWallet();
    loadContacts();
    refreshNode();
    api
      .nodeDiscover()
      .then((d: any) => (discover = d))
      .catch(() => (discover = null));
  });

  // Connection mode derived from the URL — never stored separately, so it
  // can't drift from what the app actually dials.
  const connMode = $derived(
    String(cfg.rpc_url ?? "").includes("luckylode.cc/node")
      ? "shared"
      : /127\.0\.0\.1|localhost|\[::1\]/.test(String(cfg.rpc_url ?? ""))
        ? "local"
        : "custom",
  );

  /** One-click connection presets; the backend owns URLs + credentials. */
  async function usePreset(p: "shared" | "local") {
    err = "";
    nodemsg = "";
    try {
      await api.useConnectionPreset(p);
      cfg = await api.getConfig();
      nodemsg = p === "shared" ? "using the shared testnet node" : "node on this computer — press Start node below";
      await refreshNode();
    } catch (e) {
      err = String(e);
    }
  }

  async function save() {
    err = "";
    saved = "";
    try {
      await api.setConfig({
        rpc_url: cfg.rpc_url,
        rpc_user: cfg.rpc_user,
        rpc_pass: cfg.rpc_pass,
        pool_url: cfg.pool_url,
        node_bin: cfg.node_bin ?? "",
        node_datadir: cfg.node_datadir ?? "",
        node_args: cfg.node_args ?? "-testnet",
        notifications: cfg.notifications !== false,
      });
      saved = "saved";
      setTimeout(() => (saved = ""), 1500);
    } catch (e) {
      err = String(e);
    }
  }

  async function testConn() {
    test = "testing…";
    err = "";
    try {
      const r: any = await api.testRpc();
      test = `connected · ${r.chain ?? "?"} · height ${r.height}`;
    } catch (e) {
      test = "";
      err = String(e);
    }
  }

  async function refreshNode() {
    try {
      nodestatus = await api.nodeStatus();
    } catch (e) {
      err = String(e);
    }
  }

  async function startNode() {
    nodemsg = "starting… (first launch builds the chain index, minutes)";
    err = "";
    try {
      const r: any = await api.nodeStart();
      const where = r.auto_bin ? ` (luckyd found at ${api.short(r.node_bin, 24)})` : "";
      nodemsg = `node running${where} · pid ${r.pid} · height ${r.height}`;
      await refreshNode();
    } catch (e) {
      nodemsg = "";
      err = String(e);
    }
  }

  async function stopNode() {
    nodemsg = "stopping…";
    err = "";
    try {
      const r: any = await api.nodeStop();
      nodemsg = r.killed ? `node stopped (forced, pid ${r.pid})` : `node stopped (pid ${r.pid})`;
      await refreshNode();
    } catch (e) {
      nodemsg = "";
      err = String(e);
    }
  }
</script>

<h2>Wallet</h2>
<div class="card" style="max-width:640px">
  {#if ws?.has_wallet && !ws?.locked && !ws?.legacy}
    <div class="muted">self-custody wallet · {ws.network ?? "offline"} · addresses handed out: {ws.recv_next} · locks after 15 min idle</div>
    <div class="addr" style="word-break:break-all;margin-top:6px">{ws.receive_address ?? ""}</div>
    <div class="row" style="margin-top:10px">
      {#if !revealWarn && !reveal}
        <button class="btn" onclick={() => (revealWarn = true)}>Reveal backup phrase</button>
      {:else if revealWarn && !reveal}
        <span class="muted">someone looking over your shoulder can steal everything. continue?</span>
        <button class="btn" style="border-color:var(--red);color:var(--red)" onclick={doReveal}>Show it</button>
        <button class="btn" onclick={() => (revealWarn = false)}>Cancel</button>
      {/if}
      <button class="btn" onclick={rescan}>Rescan chain</button>
      <button class="btn" onclick={lockNow}>Lock now</button>
      <label class="row" style="align-items:center;gap:8px;margin-top:10px">
        <input type="checkbox" bind:checked={cfg.notifications} style="width:auto" />
        <span class="muted">desktop notifications (block found · payout received · draw won)</span>
      </label>
    </div>
    {#if reveal}
      <div class="card" style="margin-top:10px;border-color:var(--red)">
        <div class="row" style="justify-content:space-between">
          <b>backup phrase — write it down, never share it</b>
          <span class="muted">hides in {revealLeft}s</span>
        </div>
        <div class="addr" style="word-break:break-word;margin-top:6px">{reveal}</div>
        <button class="btn" style="margin-top:8px" onclick={hideReveal}>Hide now</button>
      </div>
    {/if}
    <label for="opw" style="margin-top:12px">change spending password</label>
    <div class="row">
      <input id="opw" type="password" bind:value={oldPw} placeholder="current password" style="max-width:180px" />
      <input type="password" bind:value={newPw} placeholder="new (min 8)" style="max-width:160px" />
      <input type="password" bind:value={newPw2} placeholder="repeat new" style="max-width:160px" />
      <button class="btn" disabled={!oldPw || newPw.length < 8 || newPw !== newPw2} onclick={changePw}>Change</button>
    </div>
    {#if protectedAddrs.length > 0}
      <label style="margin-top:12px">locked for luckynode bonds (Send will not touch these)</label>
      {#each protectedAddrs as a}
        <div class="row" style="align-items:center;gap:8px">
          <span class="addr" style="word-break:break-all">{a}</span>
          {#if releaseArm === a}
            <span class="muted">release? a luckynode using it loses draw eligibility</span>
            <button class="btn" style="border-color:var(--red);color:var(--red)" onclick={() => release(a)}>Confirm</button>
            <button class="btn" onclick={() => (releaseArm = "")}>Cancel</button>
          {:else}
            <button class="btn" onclick={() => release(a)}>Release</button>
          {/if}
        </div>
      {/each}
    {/if}
    {#if contacts.length > 0}
      <label style="margin-top:12px">contacts</label>
      {#each contacts as c}
        <div class="row" style="align-items:center;gap:8px">
          {#if contactRename === c.address}
            <input
              style="max-width:160px"
              bind:value={contactName}
              placeholder="new name"
              onkeydown={(e) => { if (e.key === "Enter") saveRename(c.address); }}
            />
            <button class="btn" onclick={() => saveRename(c.address)}>Save</button>
            <button class="btn" onclick={() => { contactRename = ""; contactName = ""; }}>Cancel</button>
          {:else}
            <b style="min-width:80px">{c.name}</b>
            <span class="addr" style="word-break:break-all">{c.address}</span>
            <button class="btn" onclick={() => { contactRename = c.address; contactName = c.name; }}>Rename</button>
            <button class="btn" onclick={() => removeContact(c.address)}>Remove</button>
          {/if}
        </div>
      {/each}
    {/if}
  {:else if ws?.has_wallet && ws?.locked && !ws?.legacy}
    <div class="muted">wallet is locked — enter the spending password to continue.</div>
    <label for="lpw">spending password</label>
    <input id="lpw" type="password" bind:value={pw} spellcheck="false" onkeydown={(e) => { if (e.key === "Enter") unlock(); }} />
    <div class="row">
      <button class="btn primary" disabled={!pw} onclick={unlock}>Unlock</button>
    </div>
  {:else if ws?.legacy}
    <div class="muted">this install predates wallet encryption — set a password to seal it (plaintext is shredded).</div>
    <label for="mpw2">new spending password (min 8 characters)</label>
    <input id="mpw2" type="password" bind:value={pw} spellcheck="false" />
    <label for="mpw22">repeat password</label>
    <input id="mpw22" type="password" bind:value={pw2} spellcheck="false" />
    <div class="row">
      <button class="btn primary" disabled={pw.length < 8 || pw !== pw2} onclick={migrate}>Encrypt wallet</button>
    </div>
  {:else}
    <div class="muted">
      This wallet holds its own keys on this machine (BIP39 seed, BIP44 addresses).
      The node is only a data source and broadcast relay. Back up the 12 words — they
      are the only way to recover your funds.
    </div>
    <label for="npw">spending password (min 8 characters, encrypts the seed)</label>
    <input id="npw" type="password" bind:value={pw} spellcheck="false" />
    <label for="npw2">repeat password</label>
    <input id="npw2" type="password" bind:value={pw2} spellcheck="false" />
    <div class="row" style="margin-top:10px">
      <button class="btn primary" disabled={pw.length < 8 || pw !== pw2} onclick={create}>Create new wallet</button>
    </div>
    <label for="rest" style="margin-top:12px">or restore from backup phrase</label>
    <input id="rest" bind:value={restoreInput} placeholder="twelve words separated by spaces" spellcheck="false" />
    <label for="rpw">spending password for this install</label>
    <input id="rpw" type="password" bind:value={rpw} spellcheck="false" />
    <div class="row">
      <button class="btn" disabled={!restoreInput.trim() || rpw.length < 8} onclick={() => restore(rpw)}>Restore</button>
    </div>
  {/if}
  {#if newPhrase}
    <div class="card" style="margin-top:12px;border-color:var(--gold)">
      <b>write these 12 words down (paper, offline). anyone with them owns your funds.</b>
      <div class="addr" style="word-break:break-word;margin:8px 0;font-size:15px">{newPhrase}</div>
      <label class="row" style="align-items:center;gap:8px">
        <input type="checkbox" bind:checked={confirmBackup} style="width:auto" />
        <span class="muted">I saved the phrase somewhere safe</span>
      </label>
      {#if confirmBackup}
        <button class="btn primary" style="margin-top:8px" onclick={() => (newPhrase = null)}>Done</button>
      {/if}
    </div>
  {/if}
  {#if wmsg}<div class="muted" style="margin-top:8px">{wmsg}</div>{/if}
  {#if err}<div class="err">{err}</div>{/if}
</div>

<h2>Node</h2>
<div class="card" style="max-width:640px">
  <div class="muted">connection</div>
  <div class="row" style="gap:8px;flex-wrap:wrap;margin-top:6px">
    <button class="btn" class:primary={connMode === "shared"} onclick={() => usePreset("shared")}>Shared testnet node</button>
    <button class="btn" class:primary={connMode === "local"} onclick={() => usePreset("local")}>Node on this computer</button>
  </div>
  {#if connMode === "shared"}
    <p class="muted" style="margin:10px 0 0">
      Using Lucky Lode's shared testnet node — nothing to set up, works everywhere.
      It's a public node: staking stays off and it can't be trusted for private
      operations. Best while just trying the wallet.
    </p>
  {:else if connMode === "local"}
    <p class="muted" style="margin:10px 0 0">
      The wallet starts and stops the node here and manages its credentials
      itself — no RPC user or password to type. Data lives in
      {cfg.node_datadir || discover?.default_datadir || "the node's default directory"}.
    </p>
    <div class="row" style="margin-top:10px">
      {#if nodestatus?.managed_running}
        <button class="btn" onclick={stopNode}>Stop node (pid {nodestatus?.managed_pid})</button>
      {:else}
        <button class="btn primary" onclick={startNode}>Start node</button>
      {/if}
      <button class="btn" onclick={refreshNode}>Refresh status</button>
      {#if nodestatus}
        <span class="muted">
          {#if nodestatus.managed_running}managed · running{#if nodestatus.blocks != null} · height {nodestatus.blocks}{/if}
          {:else if nodestatus.blocks != null}a node is answering · height {nodestatus.blocks}
          {:else}no node answering{/if}
        </span>
      {/if}
    </div>
    {#if nodemsg}<div class="muted" style="margin-top:8px">{nodemsg}</div>{/if}
    {#if !cfg.node_bin}
      {#if discover?.candidates?.length}
        <div class="muted" style="margin-top:8px">
          luckyd found automatically: {discover.candidates[0]} — Start will use it
          {#if discover.candidates.length > 1}({discover.candidates.length - 1} more){/if}
        </div>
      {:else}
        <div class="muted" style="margin-top:8px">
          no luckyd found yet — install the Lucky Lode node (luckyd on your PATH),
          or set its location under Advanced.
        </div>
      {/if}
    {/if}
  {:else}
    <p class="muted" style="margin:10px 0 0">
      Custom RPC endpoint (advanced) — the fields below control it directly.
    </p>
  {/if}

  <details style="margin-top:14px">
    <summary style="cursor:pointer" class="muted">Advanced — RPC endpoint, credentials, node location</summary>
    <div style="margin-top:10px">
      <label for="rpc">node RPC URL</label>
      <input id="rpc" bind:value={cfg.rpc_url} spellcheck="false" />
      <label for="u">RPC user</label>
      <input id="u" bind:value={cfg.rpc_user} spellcheck="false" />
      <label for="p">RPC password</label>
      <input id="p" bind:value={cfg.rpc_pass} type="password" />
      <label for="pool">pool API URL</label>
      <input id="pool" bind:value={cfg.pool_url} spellcheck="false" />
      <label for="nbin">luckyd binary path (empty = auto-detect: PATH + usual install locations; legacy verusd also detected)</label>
      <input id="nbin" bind:value={cfg.node_bin} spellcheck="false" placeholder="auto-detect" />
      <label for="ndata">node datadir (empty = {discover?.default_datadir ?? "per-OS default"})</label>
      <input id="ndata" bind:value={cfg.node_datadir} spellcheck="false" />
      <label for="nargs">node flags (space-separated)</label>
      <input id="nargs" bind:value={cfg.node_args} spellcheck="false" />
      <div class="muted" style="margin-top:6px">
        The luckynode service is separate — it only registers + heartbeats.
      </div>
    </div>
  </details>

  <label for="th" style="margin-top:14px">theme</label>
  <select id="th" value={theme} onchange={onTheme}>
    <option value="dark">dark</option>
    <option value="light">light</option>
  </select>
  <div class="row" style="margin-top:14px">
    <button class="btn primary" onclick={save}>Save</button>
    <button class="btn" onclick={testConn}>Test connection</button>
    {#if saved}<span class="muted">{saved} ✓</span>{/if}
    {#if test}<span class="muted">{test}</span>{/if}
  </div>
  {#if err}<div class="err">{err}</div>{/if}
</div>

<h2>About</h2>
<div class="card muted" style="max-width:640px">
  Lucky Lode Wallet v0.1 ({api.ticker() === "LKYL" ? "mainnet" : api.ticker() === "rLKYL" ? "regtest" : "testnet"}). Open source (GPL-3.0-only); upstream attributions in NOTICE.
  This wallet holds its own 12-word seed on this machine and signs every spend locally —
  keys never touch the node. The luckynode key is a separate ed25519 identity stored on this
  machine only.
</div>

