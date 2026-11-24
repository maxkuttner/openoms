import { api } from "./client";
import type { Contract, LoadReport, ResolveRow } from "../ibkrImport";

export type { Contract, LoadReport, ResolveRow };

const base = "/admin/brokers/IBKR_TWS/instruments";
const q = (env: string) => `env=${encodeURIComponent(env)}`;

export const resolveCsv = (csv: string, env = "PAPER") =>
  api.post<ResolveRow[]>(`${base}/resolve?${q(env)}`, { csv });

export const saveConIds = (ids: number[], env = "PAPER") =>
  api.post<LoadReport>(`${base}?${q(env)}`, { con_ids: ids });

export const searchContracts = (text: string, env = "PAPER") =>
  api.get<Contract[]>(`${base}/search?q=${encodeURIComponent(text)}&${q(env)}`);
