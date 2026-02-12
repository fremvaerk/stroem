import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router";
import { Play } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Checkbox } from "@/components/ui/checkbox";
import { executeTask } from "@/lib/api";
import type { InputField } from "@/lib/types";

interface TaskRunDialogProps {
  taskName: string;
  inputSchema: Record<string, InputField>;
}

export function TaskRunDialog({ taskName, inputSchema }: TaskRunDialogProps) {
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [values, setValues] = useState<Record<string, unknown>>(() => {
    const defaults: Record<string, unknown> = {};
    for (const [key, field] of Object.entries(inputSchema)) {
      if (field.default !== undefined) {
        defaults[key] = field.default;
      } else if (field.type === "boolean") {
        defaults[key] = false;
      } else {
        defaults[key] = "";
      }
    }
    return defaults;
  });
  const [error, setError] = useState("");
  const [submitting, setSubmitting] = useState(false);

  function setValue(key: string, value: unknown) {
    setValues((prev) => ({ ...prev, [key]: value }));
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError("");
    setSubmitting(true);
    try {
      const input: Record<string, unknown> = {};
      for (const [key, val] of Object.entries(values)) {
        const field = inputSchema[key];
        if (field?.type === "number") {
          input[key] = Number(val);
        } else {
          input[key] = val;
        }
      }
      const res = await executeTask(taskName, input);
      setOpen(false);
      navigate(`/jobs/${res.job_id}`);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to execute task");
    } finally {
      setSubmitting(false);
    }
  }

  const fields = Object.entries(inputSchema);

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>
          <Play className="mr-2 h-4 w-4" />
          Run Task
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Run {taskName}</DialogTitle>
          <DialogDescription>
            {fields.length > 0
              ? "Configure input parameters and run this task."
              : "This task has no input parameters."}
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit}>
          {error && (
            <div className="mb-4 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
              {error}
            </div>
          )}
          {fields.length > 0 && (
            <div className="space-y-4 py-2">
              {fields.map(([key, field]) => (
                <div key={key} className="space-y-2">
                  <Label htmlFor={`input-${key}`}>
                    {key}
                    {field.required && (
                      <span className="ml-1 text-destructive">*</span>
                    )}
                  </Label>
                  {field.type === "boolean" ? (
                    <div className="flex items-center gap-2">
                      <Checkbox
                        id={`input-${key}`}
                        checked={!!values[key]}
                        onCheckedChange={(checked) =>
                          setValue(key, !!checked)
                        }
                      />
                      <Label
                        htmlFor={`input-${key}`}
                        className="text-sm font-normal text-muted-foreground"
                      >
                        {field.description || key}
                      </Label>
                    </div>
                  ) : (
                    <>
                      <Input
                        id={`input-${key}`}
                        type={field.type === "number" ? "number" : "text"}
                        value={String(values[key] ?? "")}
                        onChange={(e) => setValue(key, e.target.value)}
                        placeholder={field.description || key}
                        required={field.required}
                      />
                      {field.description && field.type !== "boolean" && (
                        <p className="text-xs text-muted-foreground">
                          {field.description}
                        </p>
                      )}
                    </>
                  )}
                </div>
              ))}
            </div>
          )}
          <DialogFooter className="mt-4">
            <Button
              type="button"
              variant="outline"
              onClick={() => setOpen(false)}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={submitting}>
              {submitting ? "Running..." : "Run"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
