<script lang="ts">
  import * as api from "../lib/api";
  let to = $state("");
  let amount = $state("");
  let busy = $state(false);
  let result = $state("");
  let err = $state("");

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
  <label for="to">destination address</label>
  <input id="to" bind:value={to} placeholder="L… / m… / n…" spellcheck="false" />
  <label for="amt">amount</label>
  <input id="amt" bind:value={amount} placeholder="0.0" inputmode="decimal" />
  <div class="row" style="margin-top:14px">
    <button class="btn primary" disabled={busy || !to || !amount} onclick={doSend}>
      {busy ? "sending…" : "Send"}
    </button>
    <span class="muted">fees follow the network floor (doc 11 §11.2)</span>
  </div>
  {#if result}<div class="muted" style="margin-top:10px">sent: <span class="addr">{result}</span></div>{/if}
  {#if err}<div class="err">{err}</div>{/if}
</div>
