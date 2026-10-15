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
    busy = true;
    result = "";
    err = "";
    try {
      result = await api.send(to.trim(), amount.trim());
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<h2>Send LKYL</h2>
<div class="card" style="max-width:560px">
  {#if contacts.length > 0}
    <label for="ct">contact</label>
    <select id="ct" onchange={(e) => (to = e.currentTarget.value)}>
      <option value="">— pick a contact —</option>
      {#each contacts as c}
        <option value={c.address}>{c.name} · {api.short(c.address, 10)}</option>
      {/each}
    </select>
  {/if}
  <label for="to">destination address</label>
  <input id="to" bind:value={to} placeholder="L… / m… / n…" spellcheck="false" />
  <label for="amt">amount</label>
  <input id="amt" bind:value={amount} placeholder="0.0" inputmode="decimal" />
  {#if to.trim()}
    <div class="row" style="margin-top:10px">
      <input bind:value={saveName} placeholder="save as contact…" style="max-width:220px" />
      <button class="btn" disabled={!saveName} onclick={saveContact}>Save contact</button>
      {#if saved}<span class="muted">{saved}</span>{/if}
    </div>
  {/if}
  <div class="row" style="margin-top:14px">
    <button class="btn primary" disabled={busy || !to || !amount} onclick={doSend}>
      {busy ? "sending…" : "Send"}
    </button>
    <span class="muted">fees follow the network floor (doc 11 §11.2)</span>
  </div>
  {#if result}<div class="muted" style="margin-top:10px">sent: <span class="addr">{result}</span></div>{/if}
  {#if err}<div class="err">{err}</div>{/if}
</div>
