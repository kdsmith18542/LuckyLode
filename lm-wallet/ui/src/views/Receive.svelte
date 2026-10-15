<script lang="ts">
  import * as api from "../lib/api";
  let addrs = $state<string[]>([]);
  let err = $state("");
  let copied = $state("");
  let qr = $state("");
  let qrAddr = $state("");
  let saveName = $state("");
  let saved = $state("");

  async function showQr(a: string) {
    try {
      qr = await api.addressQr(a);
      qrAddr = a;
    } catch (e) {
      err = String(e);
    }
  }
  async function gen() {
    try {
      const a = await api.newAddress();
      addrs = [a, ...addrs];
      err = "";
      await showQr(a);
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
    <span class="muted">addresses are transparent by default; shielded support lands in a later build</span>
  </div>
  {#if qr && qrAddr}
    <div class="card" style="max-width:340px;text-align:center;margin-top:12px">
      <div class="muted">receive QR</div>
      <div style="background:#fff;border-radius:8px;padding:8px;display:inline-block;margin:8px 0">
        {@html qr}
      </div>
      <div class="addr" style="word-break:break-all;font-size:12px">{qrAddr}</div>
      <div class="row" style="margin-top:8px;justify-content:center">
        <input bind:value={saveName} placeholder="save as contact…" style="max-width:180px" />
        <button class="btn" disabled={!saveName} onclick={saveContact}>Save</button>
      </div>
      {#if saved}<div class="muted">{saved}</div>{/if}
    </div>
  {/if}
  <table style="margin-top:12px">
    <tbody>
      {#each addrs as a}
        <tr>
          <td class="addr" onclick={() => copy(a)} title="click to copy">{a}</td>
          <td class="muted">{copied === a ? "copied ✓" : ""}</td>
          <td><button class="btn" onclick={() => showQr(a)}>QR</button></td>
        </tr>
      {/each}
      {#if addrs.length === 0}<tr><td class="muted">generate an address to receive</td></tr>{/if}
    </tbody>
  </table>
  {#if err}<div class="err">{err}</div>{/if}
</div>
