import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/** Format action name for display — hide synthetic `_inline:*` names. */
export function formatActionName(name: string): string {
  return name.startsWith("_inline:") ? "(inline)" : name;
}
