import { useEffect, useState } from "react";
import { Button, Indicator, Loader } from "@mantine/core";
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
        <Indicator color="red" size={8} disabled={!failedSinceSeen} offset={4}>
          <Button
            variant="light"
            size="xs"
            color={running > 0 ? "teal" : "gray"}
            leftSection={running > 0 ? <Loader size={10} color="teal" /> : undefined}
            onClick={() => setOpened(true)}
          >
            {label}
          </Button>
        </Indicator>
      )}
      <TaskCenter opened={opened} onClose={() => setOpened(false)} jobs={jobs} />
    </>
  );
}
