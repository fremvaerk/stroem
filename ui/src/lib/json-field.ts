import { REDACTED_SENTINEL } from "@/components/task/constants";
import type { InputField } from "./types";

/** How a `json` field's value reaches the server (spec D8). */
export type JsonMode = "default" | "replay" | "value";

export interface JsonFieldState {
  kind: "json";
  mode: JsonMode;
  /** Editor text — meaningful in `value` mode only. */
  text: string;
}

/** The re-run source's value for a field the source supplied. */
export interface ReplaySource {
  value: unknown;
}

export function isJsonFieldState(v: unknown): v is JsonFieldState {
  return typeof v === "object" && v !== null && (v as { kind?: unknown }).kind === "json";
}

function someString(v: unknown, pred: (s: string) => boolean): boolean {
  if (typeof v === "string") return pred(v);
  if (Array.isArray(v)) return v.some((x) => someString(x, pred));
  if (v !== null && typeof v === "object") {
    return Object.values(v as Record<string, unknown>).some((x) => someString(x, pred));
  }
  return false;
}

export const hasMask = (v: unknown) => someString(v, (s) => s.includes(REDACTED_SENTINEL));
export const hasTemplate = (v: unknown) =>
  someString(v, (s) => s.includes("{{") || s.includes("{%") || s.includes("{#"));

export function toEditorText(v: unknown): string {
  return JSON.stringify(v, null, 2);
}

/** Initial mode, first match wins (spec § 7): the source's value beats the default. */
export function initialJsonFieldState(field: InputField, source?: ReplaySource): JsonFieldState {
  if (source) {
    if (hasMask(source.value)) return { kind: "json", mode: "replay", text: "" };
    return { kind: "json", mode: "value", text: toEditorText(source.value) };
  }
  if (field.default !== undefined && hasTemplate(field.default)) {
    return { kind: "json", mode: "default", text: "" };
  }
  return {
    kind: "json",
    mode: "value",
    text: field.default === undefined ? "" : toEditorText(field.default),
  };
}

export type ParsedJson = { ok: true; value: unknown } | { ok: false; error: string };

/** Parse editor text; the error names a line and column when the engine gives a position. */
export function parseJsonText(text: string): ParsedJson {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    const lc = /line (\d+) column (\d+)/.exec(msg);
    if (lc) return { ok: false, error: `Invalid JSON: line ${lc[1]}, column ${lc[2]}` };
    const p = /position (\d+)/.exec(msg);
    if (p) {
      const pos = Number(p[1]);
      const before = text.slice(0, pos);
      const line = before.split("\n").length;
      const column = pos - before.lastIndexOf("\n");
      return { ok: false, error: `Invalid JSON: line ${line}, column ${column}` };
    }
    return { ok: false, error: "Invalid JSON" };
  }
}

/** Non-blocking notes about a value the user typed (spec § 7). */
export function valueNotes(value: unknown): string[] {
  const notes: string[] = [];
  if (hasMask(value)) {
    notes.push(`Contains ${REDACTED_SENTINEL}, which is sent as text. "Use previous value" replays the masked value instead.`);
  }
  if (hasTemplate(value)) {
    notes.push("Contains {{ … }}, which is sent as text, not evaluated.");
  }
  return notes;
}
