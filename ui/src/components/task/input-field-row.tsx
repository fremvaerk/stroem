import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Checkbox } from "@/components/ui/checkbox";
import { ComboboxField } from "@/components/task/combobox-field";
import { MultiSelectField } from "@/components/task/multi-select-field";
import { DateInput, DateTimeInput } from "@/components/task/date-input";
import type { InputField } from "@/lib/types";
import { PRIMITIVE_TYPES, REDACTED_SENTINEL } from "@/components/task/constants";
import { JsonInputField } from "@/components/task/json-input-field";
import { type JsonFieldState, type ReplaySource } from "@/lib/json-field";

export interface InputFieldRowProps {
  fieldKey: string;
  field: InputField;
  value: unknown;
  onChange: (v: unknown) => void;
  connections?: Record<string, string[]>;
  replaySource?: ReplaySource;
}

export function InputFieldRow({
  fieldKey,
  field,
  value,
  onChange,
  connections,
  replaySource,
}: InputFieldRowProps) {
  const id = `input-${fieldKey}`;

  const displayLabel = field.name ?? fieldKey;

  if (field.type === "json") {
    return (
      <JsonInputField
        id={id}
        fieldKey={fieldKey}
        field={field}
        value={value as JsonFieldState}
        onChange={onChange}
        replaySource={replaySource}
      />
    );
  }

  // Connection type input: render dropdown of available connections
  const connectionOptions = !PRIMITIVE_TYPES.has(field.type) ? connections?.[field.type] : undefined;
  if (connectionOptions && connectionOptions.length > 0) {
    return (
      <ComboboxField
        id={id}
        label={displayLabel}
        options={connectionOptions}
        value={String(value ?? "")}
        onChange={onChange}
        placeholder={`Select ${displayLabel.toLowerCase()}`}
        required={field.required}
        description={field.description || `Connection type: ${field.type}`}
      />
    );
  }

  if (field.options && field.options.length > 0) {
    if (field.multiple) {
      const arrayValue = Array.isArray(value)
        ? (value as unknown[]).map(String)
        : [];
      return (
        <MultiSelectField
          id={id}
          label={displayLabel}
          options={field.options}
          value={arrayValue}
          onChange={onChange}
          placeholder={`Select ${displayLabel.toLowerCase()}`}
          required={field.required}
          description={field.description}
          allowCustom={field.allow_custom}
        />
      );
    }
    return (
      <ComboboxField
        id={id}
        label={displayLabel}
        options={field.options}
        value={String(value ?? "")}
        onChange={onChange}
        placeholder={`Select ${displayLabel.toLowerCase()}`}
        required={field.required}
        description={field.description}
        allowCustom={field.allow_custom}
      />
    );
  }

  if (field.secret) {
    const isReplayPrefill = value === REDACTED_SENTINEL;
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {displayLabel}
          {field.required && !field.default && (
            <span className="ml-1 text-destructive">*</span>
          )}
        </Label>
        <Input
          id={id}
          type="password"
          value={String(value ?? "")}
          onChange={(e) => onChange(e.target.value)}
          placeholder={fieldKey}
          required={field.required && !field.default}
        />
        {isReplayPrefill && (
          <p className="text-xs text-muted-foreground">
            Using value from previous run — type to override.
          </p>
        )}
        {field.description && !isReplayPrefill && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }

  if (field.type === "boolean") {
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {displayLabel}
          {field.required && <span className="ml-1 text-destructive">*</span>}
        </Label>
        <div className="flex items-center gap-2">
          <Checkbox
            id={id}
            checked={!!value}
            onCheckedChange={(checked) => onChange(!!checked)}
          />
          <Label htmlFor={id} className="text-sm font-normal text-muted-foreground">
            {field.description || displayLabel}
          </Label>
        </div>
      </div>
    );
  }

  if (field.type === "text") {
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {displayLabel}
          {field.required && <span className="ml-1 text-destructive">*</span>}
        </Label>
        <Textarea
          id={id}
          rows={4}
          value={String(value ?? "")}
          onChange={(e) => onChange(e.target.value)}
          placeholder={fieldKey}
          required={field.required}
        />
        {field.description && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }

  if (field.type === "date" || field.type === "datetime") {
    const Control = field.type === "date" ? DateInput : DateTimeInput;
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {displayLabel}
          {field.required && <span className="ml-1 text-destructive">*</span>}
        </Label>
        <Control
          id={id}
          label={displayLabel}
          value={String(value ?? "")}
          onChange={onChange}
          required={field.required}
        />
        {field.description && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }

  return (
    <div className="space-y-2">
      <Label htmlFor={id}>
        {displayLabel}
        {field.required && <span className="ml-1 text-destructive">*</span>}
      </Label>
      <Input
        id={id}
        type={field.type === "number" ? "number" : "text"}
        value={String(value ?? "")}
        onChange={(e) => onChange(e.target.value)}
        placeholder={fieldKey}
        required={field.required}
      />
      {field.description && (
        <p className="text-xs text-muted-foreground">{field.description}</p>
      )}
    </div>
  );
}
