<script lang="ts">
  import * as api from "../lib/api";
  let to = $state("");
  let amount = $state("");
  let busy = $state(false);
  let result = $state("");
  let err = $state("");
  let contacts = $state<any[]>([]);
  let saveName = $state("");
  let saved = $state("");
  let preview = $state<any>(null);
  // Same-box staking shares the node's UTXO set :
  // a send can race the staker for the same coin. No coins are lost, but
  // one of the two transactions loses — warn so it is never a surprise.
  let stakingLocal = $state(false);

  async function checkStaking() {
    try {
      const st: any = await api.stakingInfo();
      const host = String(st?.node_host ?? "").toLowerCase();
      const local = ["localhost", "127.0.0.1", "::1"].includes(host);
      stakingLocal = local && !!st?.staking_on;
    } catch {
      stakingLocal = false;
    }
  }
  $effect(() => {
    checkStaking();
  });

  async function loadContacts() {
    try {
      contacts = ((await api.contactsList()) as any[]) ?? [];
    } catch {
      contacts = [];
    }
  }
  $effect(() => {
    loadContacts();
  });

  async function saveContact() {
    saved = "";
    err = "";
    try {
      await api.contactAdd(saveName, to.trim());
      saveName = "";
      saved = "contact saved ✓";
      await loadContacts();
    } catch (e) {
      err = String(e);
    }
  }

  async function doSend() {
    // Step 1: review — build + sign locally, show the exact fee/change.
    busy = true;
    result = "";
    err = "";
    preview = null;
    try {
      const r: any = await api.sendPreview(to.trim(), amount.trim());
      preview = r; // { txid, to, amount_sats, fee_sats, change_sats }
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  async function doConfirm() {
    // Step 2: confirm — broadcast the exact reviewed bytes (single-use).
    if (!preview) return;
    busy = true;
    result = "";
    err = "";
    try {
      const r: any = await api.sendConfirm(preview.txid);
      result = `sent · txid ${r.txid} · fee ${api.coinU(r.fee_sats)} · change ${api.coinU(r.change_sats)}`;
      preview = null;
      to = "";
      amount = "";
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<h2>Send {api.ticker()}</h2>
<div class="card" style="max-width:560px">
  {#if contacts.length > 0}
    <label for="ct">contact</label>
    <select id="ct" onchange={(e) => { to = e.currentTarget.value; preview = null; }}>
      <option value="">— pick a contact —</option>
      {#each contacts as c}
        <option value={c.address}>{c.name} · {api.short(c.address, 10)}</option>
      {/each}
    </select>
  {/if}
  <label for="to">destination address</label>
  <input id="to" bind:value={to} oninput={() => (preview = null)} placeholder="L… / m… / n…" spellcheck="false" />
  <label for="amt">amount</label>
  <input id="amt" bind:value={amount} oninput={() => (preview = null)} placeholder={"0.0 " + api.ticker()} inputmode="decimal" />
  {#if to.trim()}
    <div class="row" style="margin-top:10px">
      <input bind:value={saveName} placeholder="save as contact…" style="max-width:220px" />
      <button class="btn" disabled={!saveName} onclick={saveContact}>Save contact</button>
      {#if saved}<span class="muted">{saved}</span>{/if}
    </div>
  {/if}
  <div class="row" style="margin-top:14px">
    <button class="btn primary" disabled={busy || !to || !amount || preview} onclick={doSend}>
      {busy ? "building…" : "Review"}
    </button>
    <span class="muted">signed locally on this machine; only the broadcast touches the node</span>
  </div>
  {#if preview}
    <div class="card" style="margin-top:14px;border:1px solid var(--gold)">
      <b>Review before broadcast</b>
      <table>
        <tbody>
          <tr><th>to</th><td class="addr">{preview.to}</td></tr>
          <tr><th>amount</th><td>{api.coinU(preview.amount_sats)}</td></tr>
          <tr><th>miner fee</th><td>{api.coinU(preview.fee_sats)}</td></tr>
          <tr><th>change back</th><td>{api.coinU(preview.change_sats)}</td></tr>
        </tbody>
      </table>
      {#if stakingLocal}
        <div class="card" style="margin-top:10px;border:1px solid var(--gold)">
          <b>⚠ staking is running on this node</b>
          <div class="muted" style="margin-top:4px">
            These coins may also be staking. If a stake wins the same coin first, this send is orphaned and never confirms (no coins are lost); if this send wins, the node loses one stake attempt. For a guaranteed send, stop staking first (Overview → Node staking → Stop), send, then Start again.
          </div>
        </div>
      {/if}
      <div class="row" style="margin-top:10px">
        <button class="btn primary" disabled={busy} onclick={doConfirm}>Confirm &amp; send</button>
        <button class="btn" disabled={busy} onclick={() => (preview = null)}>Back</button>
      </div>
    </div>
  {/if}
  {#if result}<div class="muted" style="margin-top:10px">{result}</div>{/if}
  {#if err}<div class="err">{err}</div>{/if}
</div>
