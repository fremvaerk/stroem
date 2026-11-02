import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router";
import { restartJob, type RestartPlanResponse, type RestartTarget } from "@/lib/api";

/** A dry-run plan, bound to the job it was computed for. */
interface OpenPlan {
  jobId: string;
  target: RestartTarget;
  plan: RestartPlanResponse;
}

/**
 * Restart flow shared by the per-step "Restart from here" buttons and the
 * job header's "Restart failed steps": dry run → confirm dialog → create →
 * open the new job. `pending` is the target whose dry run is in flight.
 *
 * Every piece of state is bound to the job it belongs to. The job page does
 * not remount when it moves to another job, so a plan for job A must never
 * show (or be confirmed) on job B: state of another job reads as empty, and a
 * dry run that answers after the page left its job is dropped.
 */
export function useRestart(jobId: string) {
  const navigate = useNavigate();
  const [open, setOpen] = useState<OpenPlan | null>(null);
  const [pending, setPending] = useState<{ jobId: string; target: RestartTarget } | null>(
    null,
  );
  const [confirming, setConfirming] = useState(false);
  // Every request bumps the sequence; a response for an older one is stale.
  const seq = useRef(0);
  const currentJob = useRef(jobId);
  useEffect(() => {
    currentJob.current = jobId;
    seq.current += 1;
    // Leaving a job closes its dialog for good (returning shows no old plan).
    setOpen((o) => (o && o.jobId !== jobId ? null : o));
  }, [jobId]);

  const current = open?.jobId === jobId ? open : null;

  const start = async (target: RestartTarget) => {
    const id = ++seq.current;
    const forJob = jobId;
    setPending({ jobId: forJob, target });
    try {
      const plan = await restartJob(forJob, target, true);
      if (seq.current !== id || currentJob.current !== forJob) return;
      setOpen({ jobId: forJob, target, plan });
    } catch (err) {
      if (seq.current !== id || currentJob.current !== forJob) return;
      alert(err instanceof Error ? err.message : "Failed to plan restart");
    } finally {
      setPending((p) => (p?.jobId === forJob && p.target === target ? null : p));
    }
  };

  const close = () => {
    setOpen(null);
  };

  const confirm = async () => {
    if (!current) return;
    setConfirming(true);
    try {
      const res = await restartJob(current.jobId, current.target, false);
      setOpen(null);
      if (res.job_id) navigate(`/jobs/${res.job_id}`);
    } catch (err) {
      alert(err instanceof Error ? err.message : "Failed to restart job");
    } finally {
      setConfirming(false);
    }
  };

  return {
    target: current?.target ?? null,
    plan: current?.plan ?? null,
    pending: pending?.jobId === jobId ? pending.target : null,
    confirming,
    open: start,
    close,
    confirm,
  };
}
