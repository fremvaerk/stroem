import { useState, type KeyboardEvent, type MouseEvent } from "react";
import { ChevronsUpDown, X } from "lucide-react";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { cn } from "@/lib/utils";

export interface MultiSelectFieldProps {
  id: string;
  label: string;
  options: string[];
  value: string[];
  onChange: (v: string[]) => void;
  placeholder?: string;
  required?: boolean;
  description?: string;
  allowCustom?: boolean;
}

export function MultiSelectField({
  id,
  label,
  options,
  value,
  onChange,
  placeholder,
  required,
  description,
  allowCustom,
}: MultiSelectFieldProps) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");

  const selected = value ?? [];

  // Custom items live in `selected` but not in `options` (added via allow_custom
  // in this or a previous run). Surface them in the list so users can deselect.
  const customSelected = selected.filter((s) => !options.includes(s));
  const allItems = [...options, ...customSelected];
  const filtered = allItems.filter((opt) =>
    opt.toLowerCase().includes(search.toLowerCase()),
  );
  const showCustomRow =
    !!allowCustom &&
    search.length > 0 &&
    !allItems.some((opt) => opt.toLowerCase() === search.toLowerCase());

  function toggle(opt: string) {
    if (selected.includes(opt)) {
      onChange(selected.filter((s) => s !== opt));
    } else {
      onChange([...selected, opt]);
    }
  }

  function remove(opt: string) {
    onChange(selected.filter((s) => s !== opt));
  }

  function addCustom() {
    if (search.length === 0) return;
    if (!selected.includes(search)) {
      onChange([...selected, search]);
    }
    setSearch("");
  }

  function onTriggerKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      setOpen((o) => !o);
    }
  }

  function onChipRemoveClick(e: MouseEvent<HTMLButtonElement>, opt: string) {
    // Prevent the click from bubbling up to the trigger (which would open the popover).
    e.stopPropagation();
    remove(opt);
  }

  return (
    <div className="space-y-2">
      <Label htmlFor={id}>
        {label}
        {required && <span className="ml-1 text-destructive">*</span>}
      </Label>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <div
            id={id}
            role="combobox"
            tabIndex={0}
            aria-expanded={open}
            aria-haspopup="listbox"
            aria-label={
              selected.length > 0 ? `${label}: ${selected.join(", ")}` : label
            }
            onKeyDown={onTriggerKeyDown}
            className={cn(
              "flex w-full min-h-9 cursor-pointer items-center justify-between gap-2 rounded-md border border-input bg-transparent px-3 py-1.5 text-left text-sm shadow-sm",
              "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring",
            )}
          >
            {selected.length === 0 ? (
              <span className="text-muted-foreground">{placeholder}</span>
            ) : (
              <div className="flex flex-wrap gap-1">
                {selected.map((v) => (
                  <span
                    key={v}
                    className="inline-flex items-center gap-0.5 rounded bg-secondary py-0.5 pl-1.5 pr-0.5 text-xs font-medium text-secondary-foreground"
                  >
                    {v}
                    <button
                      type="button"
                      onClick={(e) => onChipRemoveClick(e, v)}
                      onKeyDown={(e) => {
                        // Don't let Space/Enter on the X bubble to the trigger and toggle the popover
                        if (e.key === " " || e.key === "Enter") {
                          e.stopPropagation();
                        }
                      }}
                      aria-label={`Remove ${v}`}
                      className="ml-0.5 rounded-sm p-0.5 hover:bg-muted-foreground/20 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </span>
                ))}
              </div>
            )}
            <ChevronsUpDown className="h-4 w-4 shrink-0 opacity-50" />
          </div>
        </PopoverTrigger>
        <PopoverContent
          align="start"
          className="w-[--radix-popover-trigger-width] p-0"
        >
          <Command shouldFilter={false}>
            <CommandInput
              placeholder="Search or type a value..."
              value={search}
              onValueChange={setSearch}
            />
            <CommandList>
              <CommandEmpty>No options found.</CommandEmpty>
              <CommandGroup>
                {filtered.map((opt) => {
                  const checked = selected.includes(opt);
                  return (
                    <CommandItem
                      key={opt}
                      value={opt}
                      onSelect={() => toggle(opt)}
                    >
                      <Checkbox
                        checked={checked}
                        aria-label={opt}
                        tabIndex={-1}
                        className="mr-2"
                      />
                      {opt}
                    </CommandItem>
                  );
                })}
                {showCustomRow && (
                  <CommandItem value={search} onSelect={addCustom}>
                    <Checkbox checked={false} tabIndex={-1} className="mr-2" />
                    Add &ldquo;{search}&rdquo;
                  </CommandItem>
                )}
              </CommandGroup>
            </CommandList>
          </Command>
        </PopoverContent>
      </Popover>
      {description && (
        <p className="text-xs text-muted-foreground">{description}</p>
      )}
    </div>
  );
}
