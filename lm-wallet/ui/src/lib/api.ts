// Typed wrappers over the Tauri backend commands (internal/19).
import { invoke } from "@tauri-apps/api/core";

export type Json = unknown;

export const nodeStatus = () => invoke<Json>("node_status");
export const walletBalances = () => invoke<Json>("wallet_balances");
export const transactions = (count = 50) => invoke<Json>("transactions", { count });
export const newAddress = () => invoke<string>("new_address");
export const send = (to: string, amount: string) => invoke<string>("send", { to, amount });
export const gameInfo = () => invoke<Json>("game_info");
export const nextDraw = () => invoke<Json>("next_draw");
export const luckynodes = () => invoke<Json>("luckynodes");
export const poolStats = () => invoke<Json>("pool_stats");
export const luckynodeRegister = (address: string) =>
  invoke<Json>("luckynode_register", { address });
export const serviceStart = (id: string, address: string) =>
  invoke<Json>("luckynode_service_start", { id, address });
export const serviceStop = (id: string) => invoke<Json>("luckynode_service_stop", { id });
export const serviceStatus = () => invoke<Json>("luckynode_service_status");
export const getConfig = () => invoke<Json>("get_config");
export const testRpc = () => invoke<Json>("test_rpc");
export const setConfig = (c: {
  rpc_url: string;
  rpc_user: string;
  rpc_pass: string;
  pool_url: string;
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
}) => invoke<void>("miner_start", p);
export const minerStop = () => invoke<void>("miner_stop");
export const minerStatus = () => invoke<Json>("miner_status");
export const minerLog = () => invoke<string[]>("miner_log");

/** coin display: base units → LKYL with 2-4 decimals */
export function coin(v: unknown): string {
  const n = typeof v === "number" ? v : NaN;
  if (!Number.isFinite(n)) return "–";
  return (n / 1e8).toLocaleString(undefined, { maximumFractionDigits: 4 });
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
