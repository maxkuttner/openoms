import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { notifications } from "@mantine/notifications";
import { api, ApiError } from "./client";

export function useList<T>(path: string, enabled = true) {
  return useQuery<T[]>({ queryKey: [path], queryFn: () => api.get<T[]>(path), enabled });
}

export function notifyError(err: unknown) {
  const msg = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  notifications.show({ color: "red", title: "Error", message: msg });
}

export function notifyOk(message: string) {
  notifications.show({ color: "green", message });
}

// Mutation that invalidates the given list paths and shows a toast on success/error.
export function useApiMutation<TArgs>(
  fn: (args: TArgs) => Promise<unknown>,
  opts: { invalidate: string[]; success: string; onDone?: () => void },
) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: () => {
      opts.invalidate.forEach((p) => qc.invalidateQueries({ queryKey: [p] }));
      notifyOk(opts.success);
      opts.onDone?.();
    },
    onError: notifyError,
  });
}
