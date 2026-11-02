import { useState } from "react";
import { useNavigate } from "react-router";
import { restartJob, type RestartPlanResponse, type RestartTarget } from "@/lib/api";

/**
 * Restart flow shared by the per-step "Restart from here" buttons and the
 * job header's "Restart failed steps": dry run → confirm dialog → create →
 * open the new job. `pending` is the target whose dry run is in flight.
 */
export function useRestart(jobId: string) {
  const navigate = useNavigate();
  const [target, setTarget] = useState<RestartTarget | null>(null);
  const [plan, setPlan] = useState<RestartPlanResponse | null>(null);
  const [pending, setPending] = useState<RestartTarget | null>(null);
  const [confirming, setConfirming] = useState(false);

  const open = async (next: RestartTarget) => {
    setPending(next);
    try {
      const p = await restartJob(jobId, next, true);
      setPlan(p);
      setTarget(next);
    } catch (err) {
      alert(err instanceof Error ? err.message : "Failed to plan restart");
    } finally {
      setPending(null);
    }
  };

  const close = () => {
    setTarget(null);
    setPlan(null);
  };

  const confirm = async () => {
    if (!target) return;
    setConfirming(true);
    try {
      const res = await restartJob(jobId, target, false);
      close();
      if (res.job_id) navigate(`/jobs/${res.job_id}`);
    } catch (err) {
      alert(err instanceof Error ? err.message : "Failed to restart job");
    } finally {
      setConfirming(false);
    }
  };

  return { target, plan, pending, confirming, open, close, confirm };
}
