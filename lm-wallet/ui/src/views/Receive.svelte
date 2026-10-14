<script lang="ts">
  import * as api from "../lib/api";
  let addrs = $state<string[]>([]);
  let err = $state("");
  let copied = $state("");

  async function gen() {
    try {
      const a = await api.newAddress();
      addrs = [a, ...addrs];
      err = "";
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
  <table style="margin-top:12px">
    <tbody>
      {#each addrs as a}
        <tr>
          <td class="addr" onclick={() => copy(a)} title="click to copy">{a}</td>
          <td class="muted">{copied === a ? "copied ✓" : ""}</td>
        </tr>
      {/each}
      {#if addrs.length === 0}<tr><td class="muted">generate an address to receive</td></tr>{/if}
    </tbody>
  </table>
  {#if err}<div class="err">{err}</div>{/if}
</div>
