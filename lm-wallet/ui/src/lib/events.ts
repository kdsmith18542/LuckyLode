// Tab-independent event notifications (payout received, block found,
// draw won). Polled from App.svelte on its own tick so events fire no
// matter which tab is mounted; the views keep their display loads but
// no longer evaluate notifications themselves (one evaluator, no
// duplicate backend calls, no tab dependence).
import * as api from "./api";
import { notifyOnce, seenBlockMax, balanceDelta } from "./notify";

function lastSendAt(): number {
  try {
    return Number(localStorage.getItem("ll-last-send") ?? "0") || 0;
  } catch {
    return 0;
  }
}

/** A rise right after our own send is change maturing, not a payout. */
function isOwnChangeWindow(): boolean {
  return Date.now() / 1000 - lastSendAt() < 120;
}

async function checkPayout(): Promise<void> {
  const bal: any = await api.walletBalances();
  const b = Number(bal?.balance ?? NaN);
  if (!Number.isFinite(b) || b <= 0) return;
  const delta = balanceDelta(b);
  if (delta > 0 && !isOwnChangeWindow()) {
    await notifyOnce(
      `payout-${b}`,
      "Payout received",
      `+${api.coin(delta)} ${api.ticker()} (balance ${api.coin(b)})`,
    );
  }
}

async function checkBlockFound(): Promise<void> {
  const [pool, st, cfg]: any[] = await Promise.all([
    api.poolStats().catch(() => null),
    api.minerStatus().catch(() => null),
    api.getConfig().catch(() => null),
  ]);
  // Live miner worker first (exact while mining); configured default
  // covers finds from earlier sessions. A blank worker matches nothing
  // (the old code compared against "" and never fired).
  const worker = String(st?.worker ?? cfg?.mining_worker ?? "");
  const blocks = (pool?.last_blocks ?? []) as any[];
  if (!worker || blocks.length === 0) return;
  const top = Math.max(...blocks.map((x: any) => Number(x?.height ?? 0) || 0));
  if (!seenBlockMax(top)) return;
  const mine = blocks.find(
    (x: any) =>
      Number(x?.height ?? 0) === top &&
      (x?.finder_name ?? x?.finder) === worker,
  );
  if (!mine) return;
  const bonus = Number(mine?.bonus ?? 0);
  const claim = Number(mine?.prize_claim ?? 0);
  await notifyOnce(
    `block-${top}`,
    "Block found",
    `your worker found block ${top}${bonus > 0 ? ` (+${api.coin(bonus)} bonus)` : ""}${claim > 0 ? ` (attached ${api.coin(claim)} draw claim)` : ""}`,
  );
}

async function checkDrawWon(): Promise<void> {
  let names: string[] = [];
  try {
    names = (JSON.parse(localStorage.getItem("ll-local-luckynodes") ?? "[]") as string[]).filter(
      (n) => typeof n === "string" && n.startsWith("ln-"),
    );
  } catch {
    return;
  }
  if (names.length === 0) return; // no local identities: no GBT cost
  const data: any = await api.luckynodes(names);
  const winner: string = data?.draw?.winner ?? "";
  if (!winner) return;
  const ours = new Set(
    ((data?.identities ?? []) as any[]).map((i: any) => i.address).filter(Boolean),
  );
  if (!ours.has(winner)) return;
  // The landed claim is ours only when it belongs to this epoch
  // (credit-once per epoch); otherwise omit the amount.
  const last = data?.last_claim ?? {};
  const amount =
    typeof last.amount === "number" && last.amount > 0 && last.epoch === data?.draw?.epoch
      ? ` (${api.coinU(last.amount)})`
      : "";
  await notifyOnce(
    `draw-win-${data.draw.epoch}`,
    "Luckynode Draw won",
    `your node won epoch ${data.draw.epoch}${amount}`,
  );
}

/** One pass over all three event kinds; each is independently best-effort. */
export async function pollEventNotifications(): Promise<void> {
  try {
    await checkPayout();
  } catch {
    /* advisory only */
  }
  try {
    await checkBlockFound();
  } catch {
    /* advisory only */
  }
  try {
    await checkDrawWon();
  } catch {
    /* advisory only */
  }
}

/** Stamp a user-initiated send so its returning change is not announced
 * as a payout. Call on every successful broadcast. */
export function stampOwnSend(): void {
  try {
    localStorage.setItem("ll-last-send", String(Date.now() / 1000));
  } catch {
    /* advisory only */
  }
}
