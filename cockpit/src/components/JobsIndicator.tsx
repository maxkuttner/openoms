import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { useJobs } from "../api/jobs";
import { TaskCenter } from "./TaskCenter";

/**
 * Top-bar entry to the Task Center. Hidden while nothing runs and nothing has
 * failed since the panel was last opened; otherwise "Jobs · N running", with a
 * red dot for an unseen failure. Other pages open the panel with
 * `openTaskCenter()` (an `oms:open-task-center` window event).
 */
export function JobsIndicator() {
  const { jobs, running, failedSinceSeen, markSeen } = useJobs();
  const [opened, setOpened] = useState(false);

  useEffect(() => {
    const open = () => setOpened(true);
    window.addEventListener("oms:open-task-center", open);
    return () => window.removeEventListener("oms:open-task-center", open);
  }, []);

  useEffect(() => {
    if (opened) markSeen();
  }, [opened, markSeen]);

  const label = running > 0 ? `Jobs · ${running} running` : "Jobs";
  return (
    <>
      {(running > 0 || failedSinceSeen || opened) && (
        <span className="relative">
          <Button size="xs" variant="secondary" className={cn(running > 0 && "text-buy")} onClick={() => setOpened(true)}>
            {running > 0 && <Loader2 className="animate-spin" />}
            {label}
          </Button>
          {failedSinceSeen && <span className="absolute -top-0.5 -right-0.5 size-2 rounded-full bg-sell" aria-label="A job failed" />}
        </span>
      )}
      <TaskCenter opened={opened} onClose={() => setOpened(false)} jobs={jobs} />
    </>
  );
}
