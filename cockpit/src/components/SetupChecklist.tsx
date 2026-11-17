import { Link } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { Check, Circle } from "lucide-react";
import { api } from "../api/client";
import { checklistSteps, checklistVisible, type SetupStatus } from "../checklist";

export function useSetupStatus() {
  return useQuery<SetupStatus>({
    queryKey: ["/admin/setup-status"],
    queryFn: () => api.get<SetupStatus>("/admin/setup-status"),
    refetchInterval: 15000,
  });
}

/** First-run checklist; renders nothing once every step is done. */
export function SetupChecklist() {
  const { data } = useSetupStatus();
  if (!data || !checklistVisible(data)) return null;
  return (
    <div className="rounded-md border bg-card p-3">
      <div className="mb-1.5 font-semibold">Getting started</div>
      <ul className="flex flex-col gap-1">
        {checklistSteps(data).map((s) => (
          <li key={s.key} className={s.done ? "text-muted-foreground line-through" : undefined}>
            <Link to={s.to} className="inline-flex items-center gap-1.5 hover:text-primary">
              {s.done ? <Check className="size-3.5 text-buy" /> : <Circle className="size-3.5" />}
              {s.label}
            </Link>
          </li>
        ))}
      </ul>
    </div>
  );
}
