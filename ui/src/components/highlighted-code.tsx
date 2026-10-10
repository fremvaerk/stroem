import { useMemo } from "react";
import { type CodeLanguage, highlightSegments } from "@/lib/code-language";

/** Highlighted text for a caller-styled `<pre>`; colours come from the `tok-*` rules in index.css. */
export function HighlightedCode({ code, language }: { code: string; language: CodeLanguage }) {
  const segments = useMemo(() => highlightSegments(code, language), [code, language]);
  return (
    <>
      {segments.map((s, i) => (s.cls ? <span key={i} className={s.cls}>{s.text}</span> : s.text))}
    </>
  );
}
