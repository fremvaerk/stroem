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

/**
 * Above this many styled segments (one `<span>` each), read-only views skip
 * highlighting too: a long array of short numbers is ~1 span per character.
 */
export const HIGHLIGHT_MAX_SEGMENTS = 20_000;

export interface Segment {
  text: string;
  /** `tok-*` classes from lezer's `classHighlighter`; empty for unstyled text. */
  cls: string;
}

const TOO_MANY_SEGMENTS = Symbol("too many segments");

/**
 * Splits `code` into styled segments using the same grammar and classes as the
 * editor. Lossless: the segments' text joins back to `code`, even for invalid
 * input (lezer recovers from errors instead of throwing). Over either cap the
 * whole text comes back as one plain segment.
 */
export function highlightSegments(code: string, language: CodeLanguage): Segment[] {
  const plain = [{ text: code, cls: "" }];
  if (code.length > HIGHLIGHT_MAX_CHARS) return plain;
  const segments: Segment[] = [];
  let styled = 0;
  try {
    highlightCode(
      code,
      parsers[language].parse(code),
      classHighlighter,
      (text, cls) => {
        // Stop walking as soon as the cap is passed instead of building the rest.
        if (cls && ++styled > HIGHLIGHT_MAX_SEGMENTS) throw TOO_MANY_SEGMENTS;
        segments.push({ text, cls });
      },
      () => segments.push({ text: "\n", cls: "" }),
    );
  } catch (e) {
    if (e === TOO_MANY_SEGMENTS) return plain;
    throw e;
  }
  return segments;
}
