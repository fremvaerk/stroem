import { format, parse } from "date-fns";
import { CalendarIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Checkbox } from "@/components/ui/checkbox";
import { Calendar } from "@/components/ui/calendar";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { cn } from "@/lib/utils";
import { ComboboxField } from "@/components/task/combobox-field";
import type { InputField } from "@/lib/types";
import { PRIMITIVE_TYPES, REDACTED_SENTINEL } from "@/components/task/constants";

export interface InputFieldRowProps {
  fieldKey: string;
  field: InputField;
  value: unknown;
  onChange: (v: unknown) => void;
  connections?: Record<string, string[]>;
}

export function InputFieldRow({
  fieldKey,
  field,
  value,
  onChange,
  connections,
}: InputFieldRowProps) {
  const id = `input-${fieldKey}`;

  const displayLabel = field.name ?? fieldKey;

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
        placeholder={field.description || `Select ${displayLabel.toLowerCase()}`}
        required={field.required}
        description={field.description || `Connection type: ${field.type}`}
      />
    );
  }

  if (field.options && field.options.length > 0) {
    return (
      <ComboboxField
        id={id}
        label={displayLabel}
        options={field.options}
        value={String(value ?? "")}
        onChange={onChange}
        placeholder={field.description || `Select ${displayLabel.toLowerCase()}`}
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
          placeholder={field.description || fieldKey}
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
          placeholder={field.description || fieldKey}
          required={field.required}
        />
        {field.description && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }

  if (field.type === "date") {
    const strVal = String(value ?? "");
    const dateObj = strVal
      ? parse(strVal, "yyyy-MM-dd", new Date())
      : undefined;
    const validDate =
      dateObj && !isNaN(dateObj.getTime()) ? dateObj : undefined;

    return (
      <div className="space-y-2">
        <Label>{displayLabel}{field.required && <span className="ml-1 text-destructive">*</span>}</Label>
        <Popover>
          <PopoverTrigger asChild>
            <Button
              variant="outline"
              className={cn(
                "w-full justify-start font-normal",
                !validDate && "text-muted-foreground",
              )}
            >
              <CalendarIcon className="mr-2 h-4 w-4" />
              {validDate ? validDate.toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" }) : <span>{field.description || "Pick a date"}</span>}
            </Button>
          </PopoverTrigger>
          <PopoverContent align="start" className="w-auto p-0">
            <Calendar
              mode="single"
              weekStartsOn={1}
              selected={validDate}
              onSelect={(d) => onChange(d ? format(d, "yyyy-MM-dd") : "")}
              initialFocus
            />
          </PopoverContent>
        </Popover>
        {field.description && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }

  if (field.type === "datetime") {
    // value format: "YYYY-MM-DDTHH:MM"
    const strVal = String(value ?? "");
    const [datePart, timePart] = strVal.split("T");
    const dateObj = datePart
      ? parse(datePart, "yyyy-MM-dd", new Date())
      : undefined;
    const validDate =
      dateObj && !isNaN(dateObj.getTime()) ? dateObj : undefined;
    const timeVal = timePart ?? "";

    const updateDate = (d: Date | undefined) => {
      const newDate = d ? format(d, "yyyy-MM-dd") : "";
      onChange(newDate && timeVal ? `${newDate}T${timeVal}` : newDate);
    };
    const updateTime = (t: string) => {
      const dp = validDate ? format(validDate, "yyyy-MM-dd") : "";
      onChange(dp && t ? `${dp}T${t}` : dp);
    };

    return (
      <div className="space-y-2">
        <Label>{displayLabel}{field.required && <span className="ml-1 text-destructive">*</span>}</Label>
        <div className="flex gap-2">
          <Popover>
            <PopoverTrigger asChild>
              <Button
                variant="outline"
                className={cn(
                  "flex-1 justify-start font-normal",
                  !validDate && "text-muted-foreground",
                )}
              >
                <CalendarIcon className="mr-2 h-4 w-4" />
                {validDate ? validDate.toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" }) : <span>{field.description || "Pick a date"}</span>}
              </Button>
            </PopoverTrigger>
            <PopoverContent align="start" className="w-auto p-0">
              <Calendar
                mode="single"
                weekStartsOn={1}
                selected={validDate}
                onSelect={updateDate}
                initialFocus
              />
            </PopoverContent>
          </Popover>
          <Input
            type="time"
            value={timeVal}
            onChange={(e) => updateTime(e.target.value)}
            className="w-auto"
          />
        </div>
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
        placeholder={field.description || fieldKey}
        required={field.required}
      />
      {field.description && (
        <p className="text-xs text-muted-foreground">{field.description}</p>
      )}
    </div>
  );
}
