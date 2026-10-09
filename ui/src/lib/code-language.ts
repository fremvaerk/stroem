import { classHighlighter, highlightCode } from "@lezer/highlight";
import { parser as jsonParser } from "@lezer/json";
import type { Parser } from "@lezer/common";

/**
 * Languages the UI can highlight. Adding one means a parser here and an editor
 * extension in `code-editor-impl.tsx` — both maps are keyed by this union, so
 * the compiler names whichever is missing.
 */
export type CodeLanguage = "json";

const parsers: Record<CodeLanguage, Parser> = {
  json: jsonParser,
};

/** Above this, read-only views skip highlighting: step output can be any size. */
export const HIGHLIGHT_MAX_CHARS = 200_000;

export interface Segment {
  text: string;
  /** `tok-*` classes from lezer's `classHighlighter`; empty for unstyled text. */
  cls: string;
}

/**
 * Splits `code` into styled segments using the same grammar and classes as the
 * editor. Lossless: the segments' text joins back to `code`, even for invalid
 * input (lezer recovers from errors instead of throwing).
 */
export function highlightSegments(code: string, language: CodeLanguage): Segment[] {
  if (code.length > HIGHLIGHT_MAX_CHARS) return [{ text: code, cls: "" }];
  const segments: Segment[] = [];
  highlightCode(
    code,
    parsers[language].parse(code),
    classHighlighter,
    (text, cls) => segments.push({ text, cls }),
    () => segments.push({ text: "\n", cls: "" }),
  );
  return segments;
}
