import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "./client";
import { notify } from "../components/notify";
import type { AlertRow, FutureRoot, RollIds, RollRequest } from "./types";

export function useList<T>(path: string, enabled = true) {
  return useQuery<T[]>({ queryKey: [path], queryFn: () => api.get<T[]>(path), enabled });
}

export function notifyError(err: unknown) {
  const msg = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  notify({ color: "red", title: "Error", message: msg });
}

export function notifyOk(message: string) {
  notify({ color: "green", message });
}

// Mutation that invalidates the given list paths and shows a toast on success/error.
export function useApiMutation<TArgs>(
  fn: (args: TArgs) => Promise<unknown>,
  opts: { invalidate: string[]; success: string; onDone?: (result: unknown) => void },
) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (result) => {
      opts.invalidate.forEach((p) => qc.invalidateQueries({ queryKey: [p] }));
      notifyOk(opts.success);
      opts.onDone?.(result);
    },
    onError: notifyError,
  });
}

export function useFutureRoots() {
  return useQuery<FutureRoot[]>({ queryKey: ["/futures/roots"], queryFn: () => api.get<FutureRoot[]>("/futures/roots"), staleTime: 60_000 });
}

/** Unacknowledged alerts on the portfolios the caller can view; refreshed by the live `alerts` notice. */
export function useAlerts(live: boolean) {
  return useQuery<AlertRow[]>({
    queryKey: ["/alerts"],
    queryFn: () => api.get<AlertRow[]>("/alerts"),
    refetchInterval: live ? false : 30_000,
  });
}

export function useAckAlert() {
  return useApiMutation((id: string) => api.post(`/alerts/${id}/ack`, {}), { invalidate: ["/alerts"], success: "Alert dismissed" });
}

export function useSubmitRoll(onDone?: () => void) {
  return useApiMutation((req: RollRequest) => api.post<RollIds>("/orders/roll", req), {
    invalidate: ["/orders", "/portfolios", "/alerts"],
    success: "Roll submitted",
    onDone,
  });
}
