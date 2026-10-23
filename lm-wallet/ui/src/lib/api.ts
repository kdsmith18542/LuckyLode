// Typed wrappers over the Tauri backend commands.
import { invoke } from "@tauri-apps/api/core";

export type Json = unknown;

export const nodeStatus = () => invoke<Json>("node_status");
export const nodeStart = () => invoke<Json>("node_start");
export const nodeStop = () => invoke<Json>("node_stop");
/** What the Settings connection picker shows: found node binaries (luckyd) + default datadir. */
export const nodeDiscover = () => invoke<Json>("node_discover");
/** One-click connection: "shared" (public testnet node, creds embedded) | "local" (wallet-managed node). */
export const useConnectionPreset = (preset: "shared" | "local") =>
  invoke<Json>("use_connection_preset", { preset });
export const walletBalances = () => invoke<Json>("wallet_balances");
export const transactions = (count = 50) => invoke<Json>("transactions", { count });
export const contactsList = () => invoke<Json>("contacts_list");
export const contactAdd = (name: string, address: string) =>
  invoke<void>("contact_add", { name, address });
export const contactRemove = (address: string) => invoke<void>("contact_remove", { address });
export const addressQr = (address: string, amount?: string) =>
  invoke<string>("address_qr", { address, amount: amount ?? null });
export const txDetail = (txid: string) => invoke<Json>("tx_detail", { txid });
export const newAddress = () => invoke<string>("new_address");
export const receiveList = () => invoke<Json>("receive_list");
export const walletStatus = () => invoke<Json>("wallet_status");
export const walletCreate = (password: string) => invoke<Json>("wallet_create", { password });
export const walletRestore = (phrase: string, password: string) =>
  invoke<Json>("wallet_restore", { phrase, password });
export const walletUnlock = (password: string) => invoke<Json>("wallet_unlock", { password });
export const walletLock = () => invoke<Json>("wallet_lock");
export const walletMigrate = (password: string) => invoke<Json>("wallet_migrate", { password });
export const walletChangePassword = (old_password: string, new_password: string) =>
  invoke<Json>("wallet_change_password", {
    oldPassword: old_password,
    newPassword: new_password,
  });
export const walletReveal = () => invoke<Json>("wallet_reveal");
export const walletRescan = () => invoke<Json>("wallet_rescan");
export const send = (to: string, amount: string) => invoke<Json>("send", { to, amount });
/** Review step: build + sign locally, returns the exact fee/change for confirmation. Nothing broadcasts. */
export const sendPreview = (to: string, amount: string) =>
  invoke<Json>("send_preview", { to, amount });
/** Confirm step: broadcast the exact reviewed bytes (single-use). */
export const sendConfirm = (txid: string) => invoke<Json>("send_confirm", { txid });
export const gameInfo = () => invoke<Json>("game_info");
export const emission = () => invoke<Json>("emission_info");
export const nextDraw = () => invoke<Json>("next_draw");
export const luckynodes = (names: string[]) => invoke<Json>("luckynodes", { names });
export const poolStats = () => invoke<Json>("pool_stats");
export const luckynodeRegister = (name: string, address: string) =>
  invoke<Json>("luckynode_register", { name, address });
export const serviceStart = (name: string, address: string) =>
  invoke<Json>("luckynode_service_start", { name, address });
export const serviceStop = (name: string) => invoke<Json>("luckynode_service_stop", { name });
export const serviceStatus = () => invoke<Json>("luckynode_service_status");
export const getConfig = () => invoke<Json>("get_config");
export const testRpc = () => invoke<Json>("test_rpc");
export const protectAddress = (address: string) =>
  invoke<Json>("protect_address", { address });
export const unprotectAddress = (address: string) =>
  invoke<Json>("unprotect_address", { address });
export const stakingInfo = () => invoke<Json>("staking_info");
export const setStaking = (on: boolean) => invoke<Json>("set_staking", { on });
/** Fresh address from the connected node's wallet (staking-fund target). */
export const nodeFundingAddress = () => invoke<Json>("node_funding_address");
/** Import this app's funded keys into the local node wallet (same-box staking). */
export const stakeAppCoins = () => invoke<Json>("stake_app_coins");
export const setConfig = (c: {
  rpc_url: string;
  rpc_user: string;
  rpc_pass: string;
  pool_url: string;
  notifications?: boolean;
}) => invoke<void>("set_config", c);
export const setMiningConfig = (pool_stratum: string, mining_worker: string) =>
  invoke<void>("set_mining_config", { pool_stratum, mining_worker });
export const minerStart = (p: {
  pool: string;
  worker: string;
  device: string;
  threads: number;
  duty: number;
  max_temp: number;
  gpu_platform?: number;
  gpu_device?: number;
  gpu_batch?: number;
  max_power?: number;
}) => invoke<void>("miner_start", p);
/** Solo mining against the wallet's own node (full-reward templates:
 * jackpot + pot + prize, no pool). Requires the node coinbase to pay this wallet —
 * point it there first or the backend refuses. */
export const minerSoloStart = (p: {
  device: string;
  threads: number;
  duty: number;
  max_temp: number;
  gpu_platform?: number;
  gpu_device?: number;
  gpu_batch?: number;
  max_power?: number;
}) => invoke<void>("miner_solo_start", p);
/** Write -mineraddress=<current receive address> into the Node flags
 * (takes effect on the next node restart). */
export const minerSoloPointPayouts = () => invoke<Json>("miner_solo_point_payouts");
export const minerGpus = () => invoke<Json>("miner_gpus");
export const minerStop = () => invoke<void>("miner_stop");
export const minerStatus = () => invoke<Json>("miner_status");
export const minerLog = () => invoke<string[]>("miner_log");
/** Desktop notification (Settings → Wallet toggle gates it backend-side). */
export const notify = (title: string, body: string) =>
  invoke<Json>("notify", { title, body });

/** Chain-aware ticker (no hardcoded LKYL).
 * Set once per refresh from node_status.chain; defaults to tLKYL (safe). */
let currentChain = "test";
export function setChain(c: unknown) {
  if (typeof c === "string" && c) currentChain = c;
}
export function ticker(): string {
  const c = currentChain.toLowerCase();
  if (c === "main" || c === "luckylode" || c === "mainnet") return "LKYL";
  if (c === "regtest") return "rLKYL";
  return "tLKYL";
}
/** coin + ticker suffix: the one money formatter for every money field.
 * Missing values render bare "–", never "– tLKYL". */
export function coinU(v: unknown): string {
  const c = coin(v);
  return c === "–" ? c : `${c} ${ticker()}`;
}
/** decimal-LKYL + ticker suffix. */
export function coinDecU(v: unknown): string {
  const c = coinDec(v);
  return c === "–" ? c : `${c} ${ticker()}`;
}
/** Hashrate: 846 H/s, 2.1 kH/s, 14.60 MH/s — one formatter for the
 * Miner tab and the Overview/Rewards network cards. */
export function hs(v: unknown): string {
  const n = typeof v === "number" ? v : NaN;
  if (!Number.isFinite(n)) return "–";
  if (n >= 1e6) return (n / 1e6).toFixed(2) + " MH/s";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + " kH/s";
  return Math.max(0, Math.round(n)) + " H/s";
}
/** Compact big number: 256.94, 1.2k, 3.40M. For difficulty-style values
 * that otherwise print 15 decimals. */
export function compact(v: unknown): string {
  const n = typeof v === "number" ? v : NaN;
  if (!Number.isFinite(n)) return "–";
  if (n >= 1e6) return (n / 1e6).toFixed(2) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return n >= 100 ? n.toFixed(0) : n.toFixed(2);
}
/** Thousands separators for counters (shares, rejects, hashes). */
export function int(v: unknown): string {
  const n = typeof v === "number" ? v : NaN;
  if (!Number.isFinite(n)) return "–";
  return Math.round(n).toLocaleString();
}
/** Activity direction vocabulary: one set of words everywhere
 * (Received/Sent/Change, never raw receive/send/self). */
export function dir(c: unknown): string {
  const s = String(c ?? "");
  if (s === "receive") return "Received";
  if (s === "send") return "Sent";
  if (s === "self") return "Change";
  return s || "–";
}

/** coin display: base units → exact LKYL (8dp, trailing zeros trimmed).
 * One formatter for the whole app — never show 0 for a non-zero balance
 * and never render raw floats (0.029999999999999995). Satoshis are exact. */
export function coin(v: unknown): string {
  const n = typeof v === "number" ? v : NaN;
  if (!Number.isFinite(n)) return "–";
  const s = (n / 1e8).toFixed(8).replace(/0+$/, "").replace(/\.$/, ".0");
  return Number(s).toLocaleString(undefined, { maximumFractionDigits: 8 });
}

/** decimal LKYL → 8dp trimmed. The activity feed ships floats (0.03 renders
 * as 0.029999999999999995); this rounds them to real satoshi precision. */
export function coinDec(v: unknown): string {
  const n = typeof v === "number" ? v : NaN;
  if (!Number.isFinite(n)) return "–";
  const s = n.toFixed(8).replace(/0+$/, "").replace(/\.$/, ".0");
  return Number(s).toLocaleString(undefined, { maximumFractionDigits: 8 });
}

export function short(s: unknown, n = 12): string {
  const v = String(s ?? "");
  return v.length > n + 6 ? `${v.slice(0, n)}…${v.slice(-4)}` : v || "–";
}

export function ago(ts: unknown): string {
  const t = typeof ts === "number" ? ts : 0;
  if (!t) return "–";
  const d = Math.max(0, Math.floor(Date.now() / 1000 - t));
  if (d < 60) return `${d}s`;
  if (d < 3600) return `${Math.floor(d / 60)}m`;
  if (d < 86400) return `${Math.floor(d / 3600)}h`;
  return `${Math.floor(d / 86400)}d`;
}
