import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import type { InputField } from "@/lib/types";
import {
  type JsonFieldState,
  type ReplaySource,
  hasMask,
  parseJsonText,
  toEditorText,
  valueNotes,
} from "@/lib/json-field";

export interface JsonInputFieldProps {
  id: string;
  fieldKey: string;
  field: InputField;
  value: JsonFieldState;
  onChange: (v: JsonFieldState) => void;
  replaySource?: ReplaySource;
}

/** The three-mode `json` field (spec 2026-10-06-json-input-type § 7). */
export function JsonInputField({ id, fieldKey, field, value, onChange, replaySource }: JsonInputFieldProps) {
  const label = field.name ?? fieldKey;
  const override = () => onChange({ kind: "json", mode: "value", text: "" });
  const useDefault =
    field.default !== undefined ? () => onChange({ kind: "json", mode: "default", text: "" }) : undefined;
  const usePrevious = replaySource
    ? () =>
        onChange(
          hasMask(replaySource.value)
            ? { kind: "json", mode: "replay", text: "" }
            : { kind: "json", mode: "value", text: toEditorText(replaySource.value) },
        )
    : undefined;

  const header = (
    <Label htmlFor={id}>
      {label}
      {field.required && field.default === undefined && <span className="ml-1 text-destructive">*</span>}
    </Label>
  );

  if (value.mode === "default") {
    return (
      <div className="space-y-2">
        {header}
        <pre data-testid={`json-default-${fieldKey}`} className="max-h-48 overflow-auto rounded-md border bg-muted px-3 py-2 font-mono text-xs">
          {toEditorText(field.default)}
        </pre>
        <p className="text-xs text-muted-foreground">The task&apos;s default — evaluated when the job runs.</p>
        <Button type="button" variant="outline" size="sm" onClick={override}>
          Override
        </Button>
      </div>
    );
  }

  if (value.mode === "replay") {
    return (
      <div className="space-y-2">
        {header}
        <p className="text-sm text-muted-foreground">The previous run&apos;s value (contains masked secrets) is reused.</p>
        <Button type="button" variant="outline" size="sm" onClick={override}>
          Override
        </Button>
      </div>
    );
  }

  const parsed = value.text.trim() ? parseJsonText(value.text) : null;
  const notes = parsed?.ok ? valueNotes(parsed.value) : [];
  return (
    <div className="space-y-2">
      {header}
      <Textarea
        id={id}
        rows={8}
        className="font-mono text-xs"
        value={value.text}
        placeholder={fieldKey}
        onChange={(e) => onChange({ kind: "json", mode: "value", text: e.target.value })}
      />
      {parsed && !parsed.ok && (
        <p role="alert" className="text-xs text-destructive">
          {parsed.error}
        </p>
      )}
      {notes.map((n) => (
        <p key={n} className="text-xs text-muted-foreground">
          {n}
        </p>
      ))}
      {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
      {(useDefault || usePrevious) && (
        <div className="flex gap-2">
          {useDefault && (
            <Button type="button" variant="outline" size="sm" onClick={useDefault}>
              Use default
            </Button>
          )}
          {usePrevious && (
            <Button type="button" variant="outline" size="sm" onClick={usePrevious}>
              Use previous value
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
