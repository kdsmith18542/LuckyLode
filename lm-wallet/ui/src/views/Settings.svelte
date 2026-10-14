<script lang="ts">
  import * as api from "../lib/api";
  let cfg = $state<any>({ rpc_url: "", rpc_user: "", rpc_pass: "", pool_url: "" });
  let saved = $state("");
  let err = $state("");
  let test = $state("");

  $effect(() => {
    api
      .getConfig()
      .then((c: any) => (cfg = c))
      .catch((e) => (err = String(e)));
  });

  async function save() {
    err = "";
    saved = "";
    try {
      await api.setConfig({
        rpc_url: cfg.rpc_url,
        rpc_user: cfg.rpc_user,
        rpc_pass: cfg.rpc_pass,
        pool_url: cfg.pool_url,
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
</script>

<h2>Settings</h2>
<div class="card" style="max-width:640px">
  <label for="rpc">node RPC URL (localhost by default; never expose it)</label>
  <input id="rpc" bind:value={cfg.rpc_url} spellcheck="false" />
  <label for="u">RPC user</label>
  <input id="u" bind:value={cfg.rpc_user} spellcheck="false" />
  <label for="p">RPC password</label>
  <input id="p" bind:value={cfg.rpc_pass} type="password" />
  <label for="pool">pool API URL</label>
  <input id="pool" bind:value={cfg.pool_url} spellcheck="false" />
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
  Lucky Lode Wallet v0.1 (testnet). Open source (GPL-3.0-only); upstream attributions in NOTICE.
  This wallet never sends private keys anywhere: node RPC stays local, and the luckynode key is a
  separate ed25519 identity stored on this machine only.
</div>
