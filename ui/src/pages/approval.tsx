import { useCallback } from "react";
import { useParams, Link } from "react-router";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { StatusBadge } from "@/components/status-badge";
import { ApprovalCard } from "@/components/approval-card";
import { LoadingSpinner } from "@/components/loading-spinner";
import { getJob } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import { useAsyncData } from "@/hooks/use-async-data";
import { CheckCircle2, XCircle, Clock, ExternalLink } from "lucide-react";

export function ApprovalPage() {
  const { jobId, stepName } = useParams<{ jobId: string; stepName: string }>();
  useTitle("Approval");

  const fetcher = useCallback(
    () => (jobId ? getJob(jobId) : Promise.reject("Missing job ID")),
    [jobId],
  );
  const { data: job, loading, refresh } = useAsyncData(fetcher);

  if (loading && !job) {
    return <LoadingSpinner />;
  }

  if (!job) {
    return (
      <div className="flex min-h-[60vh] items-center justify-center">
        <Card className="w-full max-w-lg">
          <CardContent className="py-12 text-center">
            <p className="text-muted-foreground">Job not found</p>
          </CardContent>
        </Card>
      </div>
    );
  }

  const step = job.steps.find((s) => s.step_name === stepName);

  if (!step) {
    return (
      <div className="flex min-h-[60vh] items-center justify-center">
        <Card className="w-full max-w-lg">
          <CardContent className="py-12 text-center">
            <p className="text-muted-foreground">
              Step &quot;{stepName}&quot; not found in this job
            </p>
            <Link
              to={`/jobs/${jobId}`}
              className="mt-3 inline-flex items-center gap-1 text-sm text-primary hover:underline"
            >
              View job details <ExternalLink className="h-3 w-3" />
            </Link>
          </CardContent>
        </Card>
      </div>
    );
  }

  const isSuspended =
    step.status === "suspended" &&
    (step.action_type === "approval" || step.action_type === "agent");
  const isCompleted = step.status === "completed";
  const isFailed = step.status === "failed";
  const isPending = step.status === "pending" || step.status === "ready";

  return (
    <div className="mx-auto max-w-2xl space-y-4 px-4 py-8">
      <Card>
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <CardTitle className="text-base">
              {job.task_name}
            </CardTitle>
            <StatusBadge status={job.status} />
          </div>
          <div className="flex items-center gap-3 text-xs text-muted-foreground">
            <span>Workspace: {job.workspace}</span>
            <span>Step: {step.step_name}</span>
          </div>
        </CardHeader>
      </Card>

      {isSuspended && (
        <ApprovalCard jobId={jobId!} step={step} onAction={refresh} />
      )}

      {isCompleted && (
        <Card className="border-green-200 bg-green-50 dark:border-green-800 dark:bg-green-950/30">
          <CardContent className="flex items-center gap-3 py-6">
            <CheckCircle2 className="h-5 w-5 text-green-600 dark:text-green-400" />
            <div>
              <p className="font-medium text-green-900 dark:text-green-200">
                Approved
              </p>
              {step.output && "approved_by" in step.output && (
                <p className="text-sm text-green-700 dark:text-green-400">
                  by {String(step.output.approved_by)}
                </p>
              )}
            </div>
          </CardContent>
        </Card>
      )}

      {isFailed && (
        <Card className="border-red-200 bg-red-50 dark:border-red-800 dark:bg-red-950/30">
          <CardContent className="py-6">
            <div className="flex items-start gap-3">
              <XCircle className="mt-0.5 h-5 w-5 text-red-600 dark:text-red-400" />
              <div>
                <p className="font-medium text-red-900 dark:text-red-200">
                  Rejected
                </p>
                {step.error_message && (
                  <p className="mt-1 text-sm text-red-700 dark:text-red-400">
                    {step.error_message}
                  </p>
                )}
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      {isPending && (
        <Card>
          <CardContent className="flex items-center gap-3 py-6">
            <Clock className="h-5 w-5 text-muted-foreground" />
            <p className="text-muted-foreground">
              This step is waiting for its dependencies to complete before
              the approval form becomes available.
            </p>
          </CardContent>
        </Card>
      )}

      <div className="text-center">
        <Link
          to={`/jobs/${jobId}`}
          className="inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground hover:underline"
        >
          View full job details <ExternalLink className="h-3 w-3" />
        </Link>
      </div>
    </div>
  );
}
