<script lang="ts">
  import * as api from "../lib/api";
  let current = $state("");
  let prev = $state<string[]>([]);
  let err = $state("");
  let copied = $state("");
  let qr = $state("");
  let qrAddr = $state("");
  let saveName = $state("");
  let saved = $state("");
  let qrAmount = $state("");

  async function load() {
    try {
      const r: any = await api.receiveList();
      current = r.current;
      prev = r.previous ?? [];
      if (!qrAddr) await showQr(r.current);
      err = "";
    } catch (e) {
      err = String(e);
    }
  }
  $effect(() => {
    load();
  });

  async function showQr(a: string) {
    try {
      qr = await api.addressQr(a, qrAmount.trim() || undefined);
      qrAddr = a;
    } catch (e) {
      err = String(e);
    }
  }
  async function gen() {
    try {
      const a = await api.newAddress();
      current = a;
      await showQr(a);
      await load();
      err = "";
    } catch (e) {
      err = String(e);
    }
  }
  async function saveContact() {
    saved = "";
    err = "";
    try {
      await api.contactAdd(saveName, qrAddr);
      saveName = "";
      saved = "contact saved ✓";
    } catch (e) {
      err = String(e);
    }
  }
  async function copy(a: string) {
    try {
      await navigator.clipboard.writeText(a);
      copied = a;
      setTimeout(() => (copied = ""), 1500);
    } catch {
      /* clipboard unavailable */
    }
  }
</script>

<h2>Receive</h2>
<div class="card" style="max-width:620px">
  <div class="row">
    <button class="btn primary" onclick={gen}>New address</button>
    <span class="muted">your addresses, derived from your own seed — previously shared addresses stay valid</span>
  </div>
  {#if qr && qrAddr}
    <div class="card" style="max-width:340px;text-align:center;margin-top:12px">
      <div class="muted">receive QR</div>
      <div style="background:#fff;border-radius:8px;padding:8px;display:inline-block;margin:8px 0">
        {@html qr}
      </div>
      <div class="addr" style="word-break:break-all;font-size:12px">{qrAddr}{qrAmount.trim() ? `?amount=${qrAmount.trim()}` : ""}</div>
      <div class="row" style="margin-top:8px;justify-content:center">
        <input bind:value={qrAmount} placeholder="amount (optional)" inputmode="decimal" style="max-width:140px" title="embeds a requested amount in the QR (payment request)" />
        <button class="btn" onclick={() => showQr(qrAddr)}>Re-make QR</button>
      </div>
      <div class="row" style="margin-top:8px;justify-content:center">
        <input bind:value={saveName} placeholder="save as contact…" style="max-width:180px" />
        <button class="btn" disabled={!saveName} onclick={saveContact}>Save</button>
      </div>
      {#if saved}<div class="muted">{saved}</div>{/if}
    </div>
  {/if}
  <table style="margin-top:12px">
    <tbody>
      {#if current}
        <tr>
          <td class="addr copyable" onclick={() => copy(current)} title="click to copy">{current}</td>
          <td class="muted">{copied === current ? "copied ✓" : "current"}</td>
          <td><button class="btn" onclick={() => showQr(current)}>QR</button></td>
        </tr>
      {/if}
      {#each prev as a}
        <tr>
          <td class="addr copyable" onclick={() => copy(a)} title="click to copy">{a}</td>
          <td class="muted">{copied === a ? "copied ✓" : ""}</td>
          <td><button class="btn" onclick={() => showQr(a)}>QR</button></td>
        </tr>
      {/each}
      {#if !current && prev.length === 0}<tr><td class="muted">no wallet yet — create or restore one to begin</td></tr>{/if}
    </tbody>
  </table>
  {#if err}<div class="err">{err}</div>{/if}
</div>
