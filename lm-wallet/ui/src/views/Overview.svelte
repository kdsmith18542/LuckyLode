<script lang="ts">
  import * as api from "../lib/api";
  import { stampOwnSend } from "../lib/events";
  let bal = $state<any>(null);
  let txs = $state<any[]>([]);
  let pool = $state<any>(null);
  let stake = $state<any>(null);
  let game = $state<any>(null);
  let em = $state<any>(null);
  let err = $state("");
  let detail = $state<any>(null);
  let loading = $state(true);
  let inFlight = $state(false);
  let showAll = $state(false);

  async function load() {
    if (inFlight) return; // one coordinator beats five racing scans
    inFlight = true;
    try {
      bal = await api.walletBalances();
      txs = ((await api.transactions(25)) as any[]) ?? [];
      pool = await api.poolStats();
      game = await api.gameInfo();
      em = await api.emission();
      err = "";
    } catch (e) {
      err = String(e);
    } finally {
      inFlight = false;
      loading = false;
    }
    try {
      stake = await api.stakingInfo();
    } catch {
      stake = null;
    }
    // Event notifications (payout / block found) are evaluated by the
    // App-level poller so they fire from any tab; this load is display
    // data only.
  }
  $effect(() => {
    load();
    const t = setInterval(load, 15000);
    return () => clearInterval(t);
  });

  let stakeBusy = $state(false);
  let importBusy = $state(false);
  let importDone = $state(false);
  // Key export is one-way (the node keeps copies indefinitely), so it
  // is two-step like every other irreversible action here.
  let confirmImport = $state(false);

  async function importKeys() {
    err = "";
    importBusy = true;
    try {
      await api.stakeAppCoins();
      importDone = true;
      confirmImport = false;
      await load();
    } catch (e) {
      err = String(e);
    } finally {
      importBusy = false;
    }
  }
  let fundBusy = $state(false);
  let fundPreview = $state<any>(null);
  let fundAmount = $state("1");
  let nodeHost = $derived((stake?.node_host as string) ?? "");
  let isLocalNode = $derived(["localhost", "127.0.0.1", "::1"].includes(nodeHost.toLowerCase()));

  async function toggleStaking() {
    err = "";
    stakeBusy = true;
    try {
      stake = await api.setStaking(!stake?.staking_on);
    } catch (e) {
      err = String(e);
    } finally {
      stakeBusy = false;
    }
  }

  async function fundNode() {
    err = "";
    fundBusy = true;
    fundPreview = null;
    try {
      const a: any = await api.nodeFundingAddress();
      const addr = a.address as string;
      if (!addr) throw new Error("node gave no address");
      const p: any = await api.sendPreview(addr, fundAmount.trim() || "1");
      fundPreview = p;
    } catch (e) {
      err = String(e);
    } finally {
      fundBusy = false;
    }
  }

  async function rebuildFundPreview() {
    if (!fundPreview) return;
    err = "";
    fundBusy = true;
    try {
      const p: any = await api.sendPreview(fundPreview.to, fundAmount.trim() || "1");
      fundPreview = p;
    } catch (e) {
      err = String(e);
    } finally {
      fundBusy = false;
    }
  }

  async function confirmFund() {
    if (!fundPreview) return;
    err = "";
    fundBusy = true;
    try {
      await api.sendConfirm(fundPreview.txid);
      stampOwnSend(); // returning change is not a payout
      fundPreview = null;
      await load();
    } catch (e) {
      err = String(e);
    } finally {
      fundBusy = false;
    }
  }



  async function openDetail(txid: string) {
    if (!txid) return;
    try {
      detail = await api.txDetail(txid);
    } catch (e) {
      err = String(e);
    }
  }

  const mining = $derived<any>(game?.mining ?? {});
  const shownTxs = $derived(showAll ? txs : txs.slice(0, 12));
</script>

<h2>Balances</h2>
{#if loading}
  <div class="grid">
    <div class="card"><div class="k">available</div><div class="v muted">loading…</div></div>
    <div class="card"><div class="k">immature</div><div class="v muted">loading…</div></div>
    <div class="card"><div class="k">unconfirmed</div><div class="v muted">loading…</div></div>
  </div>
{:else}
 <div class="grid">
  <div class="card"><div class="k">available</div><div class="v">{api.coinU(bal?.balance)}</div></div>
  <div class="card"><div class="k">immature</div><div class="v gold">{api.coinU(bal?.immature)}</div></div>
  <div class="card"><div class="k">unconfirmed</div><div class="v">{api.coinU(bal?.unconfirmed)}</div></div>
  {#if (bal?.guarded ?? 0) > 0}
  <div class="card"><div class="k">stake-guarded</div><div class="v gold" title="PoS rewards in StakeGuard outputs: consensus-valid and still staking, but no wallet can send them yet">{api.coinU(bal?.guarded)}</div></div>
  {/if}
 </div>
{/if}

<h2>Node staking — setup</h2>
<div class="card">
  {#if !stake && !err}
    <div class="muted">checking node…</div>
  {:else}
    {@const step1 = !!nodeHost}
    {@const step1local = isLocalNode}
    {@const nodeBal = stake?.node_balance ?? 0}
    {@const elig = stake?.eligible_outputs ?? 0}
    {@const unlocked = stake?.node_unlocked ?? true}
    <div class="muted" style="margin-bottom:8px">Three steps. Your app coins never leave this wallet until <b>you</b> send them — staking runs in the node wallet.</div>
    <table><tbody>
      <tr>
        <td>{step1 ? (step1local ? "✅" : "⚠️") : "⬜"}</td>
        <td><b>1 · Your own node</b><br /><span class="muted">
          {#if !step1}node unreachable — is it running?
          {:else if step1local}connected to <b>{nodeHost}</b> (this machine) — good.
          {:else}connected to <b>{nodeHost}</b> (shared) — staking there would touch someone else's node. Point Settings → Node at <b>127.0.0.1</b> (your node) to continue.
          {/if}</span></td>
      </tr>
      <tr>
        <td>{nodeBal > 0 || importDone ? "✅" : "⬜"}</td>
        <td><b>2 · Stake your coins</b><br /><span class="muted">
          {#if nodeBal > 0}node wallet holds {api.coinU(nodeBal)}.
          {:else if importDone}your app coins are imported — the node is staking them.
          {:else if step1local}
            <button class="btn primary" disabled={importBusy} onclick={() => (confirmImport = true)}>{importBusy ? "importing…" : "Stake my app coins"}</button>
            &mdash; imports your funded keys into this node (same machine; no transfer, mature coins eligible immediately).
            {#if confirmImport}
              <div class="card" style="margin-top:10px;border:1px solid var(--gold)">
                <b>Review — this copies private keys out of the app</b>
                <div class="muted" style="margin-top:4px">
                  Every funded app key is exported into the node's wallet
                  (labeled <span class="addr">lucky-lode-app</span>) so the node can
                  stake them. The node keeps those copies indefinitely —
                  only confirm on a node you fully control.
                </div>
                <div class="row" style="margin-top:10px">
                  <button class="btn primary" disabled={importBusy} onclick={importKeys}>{importBusy ? "importing…" : "Confirm & import keys"}</button>
                  <button class="btn" disabled={importBusy} onclick={() => (confirmImport = false)}>Cancel</button>
                </div>
              </div>
            {/if}
            <br /><span class="muted">or <button class="btn" disabled={fundBusy} onclick={fundNode}>{fundBusy ? "building…" : "Send coins to the node instead"}</button> (cleaner separation, but waits 150 confs).</span>
          {:else}fund the node wallet (faucet or a send to its address), then come back.
          {/if}</span></td>
      </tr>
      <tr>
        <td>{elig > 0 ? "✅" : nodeBal > 0 ? "⏳" : "⬜"}</td>
        <td><b>3 · Mature &amp; start</b><br /><span class="muted">
          {#if elig > 0}{elig} outputs · {api.coinU(stake?.eligible_balance)} eligible — ready.
          {:else if nodeBal > 0}waiting for 150-confirm maturity (~2.5 h), then Start lights up. Nothing to press.
          {:else}nothing to mature yet — finish step 2 first.
          {/if}</span>
          {#if !unlocked}<br /><span class="muted">node wallet is locked — unlock it on the node before staking.</span>{/if}
        </td>
      </tr>
    </tbody></table>
    <div class="row" style="align-items:center;gap:12px;margin-top:10px">
      <span class="muted">staking</span>
      <b>{stake?.staking_on ? "on" : "off"}</b>
      <button class="btn" disabled={stakeBusy || !(step1local && elig > 0 && unlocked) } title={!(step1local && elig > 0 && unlocked) ? "finish steps 1–3 first" : "toggle staking on your node"} onclick={toggleStaking}>{stake?.staking_on ? "Stop" : (stakeBusy ? "starting…" : "Start")}</button>
      {#if stake?.staking_on}<span class="muted">minting automatically whenever an eligible output wins a block.</span>{/if}
    </div>
  {/if}
</div>
{#if fundPreview}
    <div class="card" style="margin-top:10px;border:1px solid var(--gold)">
      <b>Fund node wallet — review</b>
      <table><tbody>
        <tr><th>to (node)</th><td class="addr">{fundPreview.to}</td></tr>
        <tr><th>amount</th><td>{api.coinU(fundPreview.amount_sats)}</td></tr>
        <tr><th>miner fee</th><td>{api.coinU(fundPreview.fee_sats)}</td></tr>
        <tr><th>change back</th><td>{api.coinU(fundPreview.change_sats)}</td></tr>
      </tbody></table>
      <div class="row" style="margin-top:8px;gap:8px;align-items:center">
        <input bind:value={fundAmount} oninput={() => (fundPreview = null)} placeholder={"amount " + api.ticker()} inputmode="decimal" style="max-width:160px" />
        <button class="btn" disabled={fundBusy} onclick={rebuildFundPreview}>Recalc</button>
        <button class="btn primary" disabled={fundBusy} onclick={confirmFund}>Confirm &amp; send</button>
        <button class="btn" disabled={fundBusy} onclick={() => (fundPreview = null)}>Cancel</button>
      </div>
    </div>
  {/if}

<h2>Recent activity</h2>
<table>
  <thead><tr><th>time</th><th>direction</th><th>amount</th><th>address</th><th>status</th></tr></thead>
  <tbody>
      {#each shownTxs as tx}
        <tr style="cursor:pointer" class:off={tx.category !== "receive"} onclick={() => openDetail(tx.txid)} title="click a row for details, txid and explorer link">
        <td>{new Date((tx.time ?? 0) * 1000).toLocaleString()}</td>
          <td>{api.dir(tx.category)}</td>
          <td>{api.coinDecU(tx.amount)}</td>
        <td class="muted">{api.short(tx.address, 10)}</td>
        <td class="muted">{tx.confirmations ?? 0} conf</td>
      </tr>
    {/each}
    {#if loading}<tr><td colspan="5" class="muted">building history cache…</td></tr>{:else if txs.length === 0}<tr><td colspan="5" class="muted">no transactions yet</td></tr>{/if}
  </tbody>
</table>
{#if txs.length > 12}
  <button class="btn" style="margin-top:6px" onclick={() => (showAll = !showAll)}>
    {showAll ? "show fewer" : `show more (${txs.length - 12} older)`}
  </button>
{/if}

{#if detail}
  <div class="card" style="max-width:720px">
    <div class="row" style="justify-content:space-between">
      <b>transaction detail</b>
      <button class="btn" onclick={() => (detail = null)}>close</button>
    </div>
    <table>
      <tbody>
        <tr><th>txid</th><td class="addr">{detail.txid} <button class="btn" onclick={() => navigator.clipboard?.writeText(detail.txid)}>copy</button> <a href={`https://luckylode.cc/explorer/tx/${detail.txid}`} target="_blank" rel="noopener">explorer ↗</a></td></tr>
        <tr><th>amount</th><td>{api.coinDecU(detail.amount)}</td></tr>
        <tr><th>fee</th><td>{api.coinDecU(detail.fee)}</td></tr>
        <tr><th>confirmations</th><td>{detail.confirmations ?? 0}</td></tr>
        <tr><th>block height</th><td>{detail.blockheight ?? "–"}</td></tr>
        <tr><th>time</th><td>{new Date((detail.time ?? 0) * 1000).toLocaleString()}</td></tr>
      </tbody>
    </table>
    {#if Array.isArray(detail.details) && detail.details.length > 0}
      <table>
        <thead><tr><th>direction</th><th>address</th><th>amount</th></tr></thead>
        <tbody>
          {#each detail.details as d}
            <tr>
              <td>{api.dir(d.category)}</td>
              <td class="muted">{api.short(d.address, 12)}</td>
              <td>{api.coinDecU(d.amount)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
{/if}

<h2>Network</h2>
<div class="grid">
  <div class="card"><div class="k">network hashrate</div><div class="v">{api.hs(mining?.networkhashps)}</div></div>
  <div class="card"><div class="k">difficulty</div><div class="v">{api.compact(mining?.difficulty)}</div></div>
  <div class="card"><div class="k">chain height</div><div class="v">{mining?.blocks ?? "–"}</div></div>
  <div class="card"><div class="k">pool hashrate 10m</div><div class="v">{api.hs(pool?.pool?.hashrate10m)}</div></div>
</div>

<h2>Emission schedule</h2>
<div class="card" style="max-width:720px">
  {#if em}
    <div class="row" style="justify-content:space-between">
      <div><span class="muted">phase</span> <strong>{em.phase}</strong> <span class="muted">(era {em.era})</span></div>
      <div>
        <span class="muted">reward</span> <strong>{api.coinU(em.reward)}</strong>
        <span class="muted">gross · {api.coinU(em.net)} after the 5% levy</span>
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
{#if err}<div class="err">{err}</div>{/if}
