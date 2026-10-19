// Best-effort desktop notifications with per-event dedupe. The backend
// `notify` command respects the Settings → Wallet toggle; this helper
// makes sure each event fires once per install (localStorage key), so
// re-polls and tab switches never double-notify.
import { notify } from "./api";

export async function notifyOnce(key: string, title: string, body: string): Promise<void> {
  try {
    const k = `ll-notify-${key}`;
    if (localStorage.getItem(k)) return;
    localStorage.setItem(k, "1");
    await notify(title, body);
  } catch {
    /* notifications are advisory — never break the UI */
  }
}

/** Highest block height seen (baseline on first sight, no notify). */
export function seenBlockMax(height: number): boolean {
  try {
    const k = "ll-notify-blockmax";
    const prev = Number(localStorage.getItem(k) ?? "0") || 0;
    if (!(height > 0)) return false;
    if (prev === 0) {
      localStorage.setItem(k, String(height));
      return false;
    }
    if (height > prev) {
      localStorage.setItem(k, String(height));
      return true;
    }
    return false;
  } catch {
    return false;
  }
}

/** Balance baseline: first sight records silently, rises report the delta. */
export function balanceDelta(now: number): number {
  try {
    const k = "ll-notify-bal";
    const raw = localStorage.getItem(k);
    if (raw === null || !(Number(raw) >= 0)) {
      localStorage.setItem(k, String(now));
      return 0;
    }
    const prev = Number(raw);
    localStorage.setItem(k, String(now));
    return now > prev ? now - prev : 0;
  } catch {
    return 0;
  }
}
