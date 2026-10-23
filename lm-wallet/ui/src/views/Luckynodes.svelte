<script lang="ts">
  import * as api from "../lib/api";
  let data = $state<any>(null);
  let svc = $state<any>(null);
  let name = $state("");
  let addr = $state("");
  let msg = $state("");
  let err = $state("");
  let loading = $state(true);

  // Identities registered from this app (ln- names). Legacy pool-era
  // ed25519 node ids stored here no longer resolve — drop them with the
  // remove button; they never affect draws (winners are addresses now).
  let localNames = $state<string[]>(
    JSON.parse(localStorage.getItem("ll-local-luckynodes") ?? "[]"),
  );
  $effect(() => {
    localStorage.setItem("ll-local-luckynodes", JSON.stringify(localNames));
  });

  async function load() {
    try {
      data = await api.luckynodes(localNames.filter((n) => n.startsWith("ln-")));
      svc = await api.serviceStatus();
      err = "";
    } catch (e) {
      err = String(e);
    } finally {
      loading = false;
    }
    // Draw-win alerts are evaluated by the App-level poller so they
    // fire from any tab; this load is display data only.
  }
  $effect(() => {
    load();
    const t = setInterval(load, 15000);
    return () => clearInterval(t);
  });

  async function register() {
    // Stage the review; Confirm broadcasts. Separate step so the
    // irreversible spend is never one click.
    msg = "";
    err = "";
    confirming = { name: name.trim(), addr: addr.trim() };
  }
  async function confirmRegister() {
    if (!confirming) return;
    msg = "";
    err = "";
    regBusy = true;
    try {
      const r: any = await api.luckynodeRegister(confirming.name, confirming.addr);
      const names = new Set(localNames);
      names.add(r.name);
      localNames = [...names];
      msg = `registered ${r.name} — update ${r.update_height}, eligible ${r.eligible ? "yes" : "not yet (mine a block)"}`;
      confirming = null;
      name = "";
      addr = "";
      await load();
    } catch (e) {
      err = String(e);
    } finally {
      regBusy = false;
    }
  }
  async function start(n: string, address: string) {
    try {
      await api.serviceStart(n, address);
      await load();
    } catch (e) {
      err = String(e);
    }
  }
  async function stop(n: string) {
    try {
      await api.serviceStop(n);
      await load();
    } catch (e) {
      err = String(e);
    }
  }
  function forget(n: string) {
    localNames = localNames.filter((x) => x !== n);
  }

  const running: string[] = $derived(svc?.running ?? []);
  const draw = $derived(data?.draw ?? {});
  const last = $derived(data?.last_claim ?? {});
  const identities = $derived<any[]>(data?.identities ?? []);
</script>

<h2>Luckynodes</h2>
<div class="grid">
  <div class="card"><div class="k">draw epoch</div><div class="v">{draw.epoch ?? "–"}{data?.epoch_len ? ` · ${data.epoch_len} blocks/epoch` : ""}</div></div>
  <div class="card"><div class="k">epoch budget</div><div class="v gold">{typeof draw.budget === "number" ? api.coinU(draw.budget) : "–"}</div></div>
  <div class="card"><div class="k">remaining</div><div class="v">{typeof draw.remaining === "number" ? api.coinU(draw.remaining) : "–"}</div></div>
  <div class="card"><div class="k">last claim</div><div class="v">{typeof last.amount === "number" && last.amount > 0 ? `${api.coinU(last.amount)} (epoch ${last.epoch})` : "–"}</div></div>
</div>
<div class="muted" style="margin-top:6px">
  Every epoch the chain picks a winner from registered
  <span class="addr">ln-*</span> identities, and the winning block pays them
  straight from the prize fund — no pool custody involved. Registration is
  a one-time on-chain step (100 LKYL fee from the connected node's
  wallet); staying eligible takes a small update transaction each epoch.
</div>

<table style="margin-top:14px">
  <thead>
    <tr><th>identity</th><th>payout address</th><th>on chain</th><th>updated</th><th>eligible</th><th>local service</th><th></th></tr>
  </thead>
  <tbody>
    {#each identities as n}
      <tr>
        <td>{api.short(n.name, 14)}</td>
        <td class="muted">{api.short(n.address, 10)}</td>
        <td>{n.exists ? `yes @${n.update_height ?? "?"}` : (n.error ? "error" : "no")}</td>
        <td class="muted">{n.update_height ? `#${n.update_height}` : "–"}</td>
        <td><span class="pill" class:ok={n.eligible} class:off={!n.eligible}>{n.eligible ? "yes" : "idle"}</span></td>
        <td>
          {#if running.includes(n.name)}
            <button class="btn danger" onclick={() => stop(n.name)}>Stop</button>
          {:else if n.exists}
            <button class="btn" onclick={() => start(n.name, n.address)}>Start</button>
          {:else}
            <span class="muted">register first</span>
          {/if}
        </td>
        <td><button class="btn" onclick={() => forget(n.name)}>forget</button></td>
      </tr>
    {/each}
    {#if loading && identities.length === 0}
      <tr><td colspan="7" class="muted">loading identities…</td></tr>
    {:else if identities.length === 0}
      <tr><td colspan="7" class="muted">no local identities — register one below</td></tr>
    {/if}
  </tbody>
</table>

<div class="muted" style="margin-top:6px">
  the local service runs only while this app is open — closing the wallet stops heartbeats
  (the identity goes idle and sits out draws until you start it again). For 24/7 participation run
  the standalone <span class="addr">luckynode</span> daemon on an always-on machine instead.
</div>

<h2>Register a luckynode identity</h2>
<div class="card" style="max-width:640px">
  <label for="lname">identity name (ln- namespace)</label>
  <input id="lname" bind:value={name} oninput={() => (confirming = null)} placeholder="ln-foo" spellcheck="false" />
  <label for="addr" style="margin-top:10px">payout address (draw winnings pay this address)</label>
  <input id="addr" bind:value={addr} oninput={() => (confirming = null)} placeholder="L… / m… / n…" spellcheck="false" />
  <div class="row" style="margin-top:12px">
  <button class="btn primary" disabled={!name || !addr || regBusy} onclick={register}>Register on chain</button>
  <span class="muted">name commitment + identity registration, funded from the connected node's wallet (identity fee set by the chain — 100 tLKYL on the current testnet)</span>
  </div>
  <div class="muted" style="margin-top:8px">
    The commitment's coins stay in the connected node's wallet, not this app.
    A pending registration can therefore show as a debit under this app's
    <b>unconfirmed</b> balance while the same value sits in the node wallet
    (Overview → Node staking). Retrying a pending step resumes it — it does
    not pay a second commitment.
  </div>
  {#if confirming}
    <div class="card" style="margin-top:12px;border:1px solid var(--gold)">
      <b>Review — this spends real chain fees</b>
      <table><tbody>
        <tr><th>identity</th><td class="addr">{confirming.name}</td></tr>
        <tr><th>payout address</th><td class="addr">{confirming.addr}</td></tr>
        <tr><th>funded from</th><td class="muted">the connected node's wallet (not this app)</td></tr>
      </tbody></table>
      <div class="row" style="margin-top:10px">
        <button class="btn primary" disabled={regBusy} onclick={confirmRegister}>{regBusy ? "registering…" : "Confirm & register"}</button>
        <button class="btn" disabled={regBusy} onclick={() => (confirming = null)}>Back</button>
      </div>
    </div>
  {/if}
  {#if msg}<div class="muted" style="margin-top:10px">{msg}</div>{/if}
</div>
{#if err}<div class="err" role="alert">{err}</div>{/if}
