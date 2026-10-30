import { useEffect, useRef } from "react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { humanSize } from "@/components/artifact-helpers";
import type { FileFieldState } from "@/lib/file-field";
import type { InputField } from "@/lib/types";
import { uploadInputFile } from "@/lib/upload";

export interface FileInputFieldProps {
  id: string;
  fieldKey: string;
  field: InputField;
  value: FileFieldState;
  onChange: (v: FileFieldState) => void;
  workspace: string;
  /** The task's `id` (not the route param), like `executeTask`. */
  taskId: string;
  /** Re-run source job: a Replace upload names it so a pinned source is checked. */
  sourceJobId?: string;
}

/** A `file` input field: picks, uploads at once, shows progress (spec 2026-10-29-file-inputs § 7.1). */
export function FileInputField({ id, fieldKey, field, value, onChange, workspace, taskId, sourceJobId }: FileInputFieldProps) {
  const label = field.name ?? fieldKey;
  const inputRef = useRef<HTMLInputElement>(null);
  const abortRef = useRef<AbortController | null>(null);

  useEffect(() => () => abortRef.current?.abort(), []);

  async function start(file: File) {
    abortRef.current?.abort();
    const ac = new AbortController();
    abortRef.current = ac;
    onChange({ kind: "file", mode: "uploading", name: file.name, size: file.size, progress: 0 });
    try {
      const res = await uploadInputFile(workspace, taskId, file, {
        sourceJobId,
        signal: ac.signal,
        onProgress: (progress) => {
          if (!ac.signal.aborted) {
            onChange({ kind: "file", mode: "uploading", name: file.name, size: file.size, progress });
          }
        },
      });
      if (ac.signal.aborted) return;
      onChange({
        kind: "file",
        mode: "ready",
        fileId: res.file_id,
        name: res.name,
        size: res.size_bytes,
        fromSource: false,
      });
    } catch (e) {
      if (ac.signal.aborted) return;
      onChange({
        kind: "file",
        mode: "failed",
        name: file.name,
        error: e instanceof Error ? e.message : "Upload failed",
      });
    }
  }

  function remove() {
    abortRef.current?.abort();
    onChange({ kind: "file", mode: "empty" });
  }

  const choose = () => inputRef.current?.click();

  return (
    <div className="space-y-2">
      <Label htmlFor={id}>
        {label}
        {field.required && <span className="ml-1 text-destructive">*</span>}
      </Label>
      <input
        ref={inputRef}
        id={id}
        type="file"
        className="sr-only"
        tabIndex={-1}
        onChange={(e) => {
          const f = e.target.files?.[0];
          e.target.value = "";
          if (f) void start(f);
        }}
      />

      {value.mode === "empty" && (
        <Button type="button" variant="outline" size="sm" onClick={choose}>
          Choose file
        </Button>
      )}

      {value.mode === "uploading" && (
        <div className="space-y-1">
          <div className="flex items-center gap-2 text-sm">
            <span className="truncate font-mono">{value.name}</span>
            <span className="tabular-nums text-xs text-muted-foreground">{humanSize(value.size)}</span>
          </div>
          <div
            role="progressbar"
            aria-label={`Uploading ${value.name}`}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(value.progress * 100)}
            className="h-2 w-full overflow-hidden rounded bg-muted"
          >
            <div className="h-full bg-primary" style={{ width: `${Math.round(value.progress * 100)}%` }} />
          </div>
          <Button type="button" variant="outline" size="sm" onClick={remove}>
            Cancel
          </Button>
        </div>
      )}

      {value.mode === "failed" && (
        <div className="space-y-1">
          <div className="text-sm">
            <span className="truncate font-mono">{value.name}</span>
          </div>
          <p role="alert" className="text-xs text-destructive">
            {value.error}
          </p>
          <Button type="button" variant="outline" size="sm" onClick={choose}>
            Choose file
          </Button>
        </div>
      )}

      {value.mode === "ready" && (
        <div className="space-y-1">
          <div className="flex items-center gap-2 text-sm">
            <span className="truncate font-mono">{value.name}</span>
            <span className="tabular-nums text-xs text-muted-foreground">{humanSize(value.size)}</span>
          </div>
          {value.fromSource && <p className="text-xs text-muted-foreground">From the previous run</p>}
          <div className="flex gap-2">
            <Button type="button" variant="outline" size="sm" onClick={choose}>
              Replace
            </Button>
            <Button type="button" variant="outline" size="sm" onClick={remove}>
              Remove
            </Button>
          </div>
        </div>
      )}

      {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
    </div>
  );
}
