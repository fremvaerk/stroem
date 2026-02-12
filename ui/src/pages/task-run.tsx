import { useEffect, useState, type FormEvent } from "react";
import { useParams, useNavigate, Link } from "react-router";
import { ArrowLeft } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Checkbox } from "@/components/ui/checkbox";
import { getTask, executeTask } from "@/lib/api";
import type { TaskDetail, InputField } from "@/lib/types";

export function TaskRunPage() {
  const { workspace, name } = useParams<{ workspace: string; name: string }>();
  const navigate = useNavigate();
  const [task, setTask] = useState<TaskDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState("");
  const [values, setValues] = useState<Record<string, unknown>>({});
  const [error, setError] = useState("");
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    if (!workspace || !name) return;
    let cancelled = false;

    async function load() {
      try {
        const data = await getTask(workspace!, name!);
        if (!cancelled) {
          setTask(data);
          const defaults: Record<string, unknown> = {};
          for (const [key, field] of Object.entries(data.input)) {
            if (field.default !== undefined) {
              defaults[key] = field.default;
            } else if (field.type === "boolean") {
              defaults[key] = false;
            } else {
              defaults[key] = "";
            }
          }
          setValues(defaults);
        }
      } catch (err) {
        if (!cancelled)
          setLoadError(err instanceof Error ? err.message : "Failed to load task");
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    return () => {
      cancelled = true;
    };
  }, [workspace, name]);

  function setValue(key: string, value: unknown) {
    setValues((prev) => ({ ...prev, [key]: value }));
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!workspace || !task) return;
    setError("");
    setSubmitting(true);
    try {
      const input: Record<string, unknown> = {};
      for (const [key, val] of Object.entries(values)) {
        const field = task.input[key];
        if (field?.type === "number") {
          input[key] = Number(val);
        } else {
          input[key] = val;
        }
      }
      const res = await executeTask(workspace, task.name, input);
      navigate(`/jobs/${res.job_id}`);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to execute task");
    } finally {
      setSubmitting(false);
    }
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  if (loadError || !task) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">{loadError || "Task not found"}</p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/tasks">Back to tasks</Link>
        </Button>
      </div>
    );
  }

  const fields = Object.entries(task.input);

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to={`/workspaces/${encodeURIComponent(workspace!)}/tasks/${encodeURIComponent(name!)}`}>
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <h1 className="text-2xl font-semibold tracking-tight">
          Run {task.name}
        </h1>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {fields.length > 0
              ? "Configure input parameters"
              : "This task has no input parameters"}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit}>
            {error && (
              <div className="mb-4 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
                {error}
              </div>
            )}
            {fields.length > 0 && (
              <div className="space-y-4">
                {fields.map(([key, field]) => (
                  <InputFieldRow
                    key={key}
                    fieldKey={key}
                    field={field}
                    value={values[key]}
                    onChange={(v) => setValue(key, v)}
                  />
                ))}
              </div>
            )}
            <div className="mt-6 flex gap-3">
              <Button type="submit" disabled={submitting}>
                {submitting ? "Running..." : "Run Task"}
              </Button>
              <Button type="button" variant="outline" asChild>
                <Link to={`/workspaces/${encodeURIComponent(workspace!)}/tasks/${encodeURIComponent(name!)}`}>
                  Cancel
                </Link>
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}

function InputFieldRow({
  fieldKey,
  field,
  value,
  onChange,
}: {
  fieldKey: string;
  field: InputField;
  value: unknown;
  onChange: (v: unknown) => void;
}) {
  const id = `input-${fieldKey}`;

  if (field.type === "boolean") {
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {fieldKey}
          {field.required && <span className="ml-1 text-destructive">*</span>}
        </Label>
        <div className="flex items-center gap-2">
          <Checkbox
            id={id}
            checked={!!value}
            onCheckedChange={(checked) => onChange(!!checked)}
          />
          <Label htmlFor={id} className="text-sm font-normal text-muted-foreground">
            {field.description || fieldKey}
          </Label>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-2">
      <Label htmlFor={id}>
        {fieldKey}
        {field.required && <span className="ml-1 text-destructive">*</span>}
      </Label>
      <Input
        id={id}
        type={field.type === "number" ? "number" : "text"}
        value={String(value ?? "")}
        onChange={(e) => onChange(e.target.value)}
        placeholder={field.description || fieldKey}
        required={field.required}
      />
      {field.description && (
        <p className="text-xs text-muted-foreground">{field.description}</p>
      )}
    </div>
  );
}
