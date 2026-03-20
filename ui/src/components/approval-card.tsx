import { useState } from "react";
import { CheckCircle2, XCircle } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Checkbox } from "@/components/ui/checkbox";
import { ComboboxField } from "@/components/task/combobox-field";
import { approveStep } from "@/lib/api";
import { formatTime } from "@/lib/formatting";
import type { InputField, JobStep } from "@/lib/types";

interface ApprovalCardProps {
  jobId: string;
  step: JobStep;
  onAction: () => void;
}

export function ApprovalCard({ jobId, step, onAction }: ApprovalCardProps) {
  const [approvalInput, setApprovalInput] = useState<Record<string, unknown>>({});
  const [rejecting, setRejecting] = useState(false);
  const [rejectionReason, setRejectionReason] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const approvalFields = step.approval_fields as Record<string, InputField> | null;
  const hasFields = approvalFields && Object.keys(approvalFields).length > 0;

  async function handleApprove() {
    setError(null);

    // Validate required fields before submitting
    if (approvalFields) {
      for (const [key, field] of Object.entries(approvalFields)) {
        const f = field as InputField;
        if (f.required && (approvalInput[key] === undefined || approvalInput[key] === null || approvalInput[key] === "")) {
          setError(`Required field '${f.name ?? key}' is missing`);
          return;
        }
      }
    }

    setLoading(true);
    try {
      await approveStep(jobId, step.step_name, true, hasFields ? approvalInput : undefined);
      onAction();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to approve step");
    } finally {
      setLoading(false);
    }
  }

  async function handleReject() {
    setLoading(true);
    setError(null);
    try {
      await approveStep(
        jobId,
        step.step_name,
        false,
        undefined,
        rejectionReason.trim() || undefined,
      );
      onAction();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to reject step");
    } finally {
      setLoading(false);
    }
  }

  return (
    <Card className="border-amber-200 bg-amber-50 dark:border-amber-800 dark:bg-amber-950/30">
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center gap-2 text-base text-amber-900 dark:text-amber-200">
          Awaiting Approval
        </CardTitle>
        {step.suspended_at && (
          <p className="text-xs text-muted-foreground">
            Waiting since {formatTime(step.suspended_at)}
          </p>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        {step.approval_message && (
          <pre className="whitespace-pre-wrap break-words rounded-md border border-amber-200 bg-white p-3 font-mono text-sm text-foreground dark:border-amber-800 dark:bg-background">
            {step.approval_message}
          </pre>
        )}

        {hasFields && !rejecting && (
          <div className="space-y-3">
            {Object.entries(approvalFields).map(([key, field]) => {
              const id = `approval-field-${key}`;
              const label = field.name ?? key;
              const value = approvalInput[key];

              // options: use combobox (no custom input for approval fields)
              if (field.options && field.options.length > 0) {
                return (
                  <ComboboxField
                    key={key}
                    id={id}
                    label={label}
                    options={field.options}
                    value={String(value ?? "")}
                    onChange={(v) =>
                      setApprovalInput((prev) => ({ ...prev, [key]: v }))
                    }
                    placeholder={field.description ?? key}
                    required={field.required}
                    description={field.description}
                  />
                );
              }

              // boolean: use checkbox
              if (field.type === "boolean") {
                return (
                  <div key={key} className="space-y-1.5">
                    <Label htmlFor={id}>
                      {label}
                      {field.required && (
                        <span className="ml-1 text-destructive">*</span>
                      )}
                    </Label>
                    <div className="flex items-center gap-2">
                      <Checkbox
                        id={id}
                        checked={!!value}
                        onCheckedChange={(checked) =>
                          setApprovalInput((prev) => ({
                            ...prev,
                            [key]: !!checked,
                          }))
                        }
                      />
                      {field.description && (
                        <Label
                          htmlFor={id}
                          className="text-sm font-normal text-muted-foreground"
                        >
                          {field.description}
                        </Label>
                      )}
                    </div>
                  </div>
                );
              }

              // text: multiline textarea
              if (field.type === "text") {
                return (
                  <div key={key} className="space-y-1.5">
                    <Label htmlFor={id}>
                      {label}
                      {field.required && (
                        <span className="ml-1 text-destructive">*</span>
                      )}
                    </Label>
                    <Textarea
                      id={id}
                      rows={4}
                      value={String(value ?? "")}
                      onChange={(e) =>
                        setApprovalInput((prev) => ({
                          ...prev,
                          [key]: e.target.value,
                        }))
                      }
                      placeholder={field.description ?? key}
                    />
                    {field.description && (
                      <p className="text-xs text-muted-foreground">
                        {field.description}
                      </p>
                    )}
                  </div>
                );
              }

              // default: single-line input (string, number, etc.)
              return (
                <div key={key} className="space-y-1.5">
                  <Label htmlFor={id}>
                    {label}
                    {field.required && (
                      <span className="ml-1 text-destructive">*</span>
                    )}
                  </Label>
                  <Input
                    id={id}
                    type={field.type === "number" ? "number" : "text"}
                    value={String(value ?? "")}
                    onChange={(e) =>
                      setApprovalInput((prev) => ({
                        ...prev,
                        [key]: e.target.value,
                      }))
                    }
                    placeholder={field.description ?? key}
                  />
                  {field.description && (
                    <p className="text-xs text-muted-foreground">
                      {field.description}
                    </p>
                  )}
                </div>
              );
            })}
          </div>
        )}

        {rejecting && (
          <div className="space-y-1.5">
            <Label htmlFor="rejection-reason">
              Rejection reason{" "}
              <span className="text-muted-foreground">(optional)</span>
            </Label>
            <Textarea
              id="rejection-reason"
              rows={3}
              value={rejectionReason}
              onChange={(e) => setRejectionReason(e.target.value)}
              placeholder="Describe why this step is being rejected..."
              autoFocus
            />
          </div>
        )}

        {error && (
          <p className="rounded-md border border-red-200 bg-red-50 px-3 py-2 text-sm text-red-700 dark:border-red-800 dark:bg-red-950/30 dark:text-red-400">
            {error}
          </p>
        )}

        <div className="flex items-center gap-2">
          {!rejecting ? (
            <>
              <Button
                size="sm"
                className="bg-green-600 text-white hover:bg-green-700 dark:bg-green-700 dark:hover:bg-green-600"
                disabled={loading}
                onClick={handleApprove}
              >
                <CheckCircle2 className="mr-1.5 h-3.5 w-3.5" />
                {loading ? "Approving..." : "Approve"}
              </Button>
              <Button
                size="sm"
                variant="outline"
                className="border-red-300 text-red-700 hover:bg-red-50 dark:border-red-700 dark:text-red-400 dark:hover:bg-red-950/30"
                disabled={loading}
                onClick={() => setRejecting(true)}
              >
                <XCircle className="mr-1.5 h-3.5 w-3.5" />
                Reject
              </Button>
            </>
          ) : (
            <>
              <Button
                size="sm"
                variant="destructive"
                disabled={loading}
                onClick={handleReject}
              >
                <XCircle className="mr-1.5 h-3.5 w-3.5" />
                {loading ? "Rejecting..." : "Confirm Rejection"}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                disabled={loading}
                onClick={() => {
                  setRejecting(false);
                  setRejectionReason("");
                }}
              >
                Cancel
              </Button>
            </>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
