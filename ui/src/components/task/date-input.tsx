import { useEffect, useRef, useState } from "react";
import { max, min, startOfMonth } from "date-fns";
import { CalendarIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverAnchor,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { cn } from "@/lib/utils";
import { formatIsoDate, parseIsoDate } from "@/lib/iso-date";

// DayPicker's own dropdown range ends with the current year, which would hide
// future dates (schedules, deadlines).
const YEARS_BACK = 100;
const YEARS_AHEAD = 20;
const INVALID_MESSAGE = "Expected a date as YYYY-MM-DD, e.g. 2026-10-07";

export interface DateInputProps {
  id: string;
  /** Field label, for the calendar button's accessible name. */
  label: string;
  /** `YYYY-MM-DD`, or `""`. */
  value: string;
  /** Called with `YYYY-MM-DD`, or `""` while the text is empty or not a date. */
  onChange: (value: string) => void;
  required?: boolean;
  className?: string;
}

export function DateInput({
  id,
  label,
  value,
  onChange,
  required,
  className,
}: DateInputProps) {
  const [text, setText] = useState(value);
  // The value this component last emitted or adopted. A different `value` came
  // from the parent (re-run prefill) and replaces the text; the parent echoing
  // our own emission does not, so half-typed text survives the round trip.
  const [seen, setSeen] = useState(value);
  if (value !== seen) {
    setSeen(value);
    setText(value);
  }
  const [focused, setFocused] = useState(false);
  const [open, setOpen] = useState(false);
  const [month, setMonth] = useState(() => parseIsoDate(value) ?? new Date());
  const inputRef = useRef<HTMLInputElement>(null);

  const selected = parseIsoDate(text);
  const invalid = text.trim() !== "" && !selected;
  const showError = invalid && !focused;
  const errorId = `${id}-error`;

  // Native form validation, so the Run form refuses to submit text it would
  // otherwise send as an empty value.
  useEffect(() => {
    inputRef.current?.setCustomValidity(invalid ? INVALID_MESSAGE : "");
  }, [invalid]);

  const emit = (next: string) => {
    setSeen(next);
    onChange(next);
  };

  const handleText = (next: string) => {
    setText(next);
    const date = parseIsoDate(next);
    if (date) setMonth(date);
    emit(date ? formatIsoDate(date) : "");
  };

  const pick = (date: Date) => {
    const iso = formatIsoDate(date);
    setText(iso);
    emit(iso);
    setOpen(false);
  };

  const handleOpenChange = (next: boolean) => {
    if (next) setMonth(selected ?? new Date());
    setOpen(next);
  };

  const today = new Date();
  const shown = startOfMonth(selected ?? today);
  const startMonth = min([new Date(today.getFullYear() - YEARS_BACK, 0), shown]);
  const endMonth = max([new Date(today.getFullYear() + YEARS_AHEAD, 11), shown]);

  return (
    <div className={cn("space-y-1", className)}>
      <Popover open={open} onOpenChange={handleOpenChange}>
        <PopoverAnchor asChild>
          <div className="relative">
            <Input
              ref={inputRef}
              id={id}
              value={text}
              placeholder="YYYY-MM-DD"
              autoComplete="off"
              spellCheck={false}
              required={required}
              aria-invalid={showError || undefined}
              aria-describedby={showError ? errorId : undefined}
              className="pr-10 aria-invalid:border-destructive aria-invalid:focus-visible:ring-destructive"
              onChange={(e) => handleText(e.target.value)}
              onFocus={() => setFocused(true)}
              onBlur={() => {
                setFocused(false);
                // Show what will be sent, e.g. a pasted value with stray spaces.
                if (selected) setText(formatIsoDate(selected));
              }}
            />
            <PopoverTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label={`Open calendar for ${label}`}
                className="absolute right-0.5 top-1/2 h-8 w-8 -translate-y-1/2 text-muted-foreground"
              >
                <CalendarIcon className="h-4 w-4" />
              </Button>
            </PopoverTrigger>
          </div>
        </PopoverAnchor>
        <PopoverContent align="start" className="w-auto p-0">
          <Calendar
            mode="single"
            required
            weekStartsOn={1}
            captionLayout="dropdown"
            startMonth={startMonth}
            endMonth={endMonth}
            month={month}
            onMonthChange={setMonth}
            selected={selected}
            onSelect={pick}
            autoFocus
          />
          <div className="border-t p-2">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="w-full"
              onClick={() => pick(new Date())}
            >
              Today
            </Button>
          </div>
        </PopoverContent>
      </Popover>
      {showError && (
        <p id={errorId} className="text-xs text-destructive">
          {INVALID_MESSAGE}
        </p>
      )}
    </div>
  );
}

/**
 * A `datetime` value, `YYYY-MM-DDTHH:MM`: an ISO date box plus the native time
 * box. The time is kept locally so it survives the date being half-typed (the
 * combined value is `""` meanwhile).
 */
export function DateTimeInput({
  id,
  label,
  value,
  onChange,
  required,
}: Omit<DateInputProps, "className">) {
  const [datePart, timePart = ""] = value.split("T");
  const [time, setTime] = useState(timePart);
  const [seen, setSeen] = useState(value);
  if (value !== seen) {
    setSeen(value);
    setTime(timePart);
  }

  const emit = (date: string, nextTime: string) => {
    const next = date && nextTime ? `${date}T${nextTime}` : date;
    setSeen(next);
    onChange(next);
  };

  return (
    <div className="flex items-start gap-2">
      <DateInput
        id={id}
        label={label}
        value={datePart}
        onChange={(date) => emit(date, time)}
        // A time alone would be submitted as nothing at all.
        required={required || time !== ""}
        className="flex-1"
      />
      <Input
        type="time"
        aria-label={`${label} time`}
        value={time}
        onChange={(e) => {
          setTime(e.target.value);
          emit(datePart, e.target.value);
        }}
        className="w-auto"
      />
    </div>
  );
}
