import { lazy, Suspense } from "react";
import { Textarea } from "@/components/ui/textarea";
import type { CodeLanguage } from "@/lib/code-language";
import { importOr } from "@/lib/lazy-fallback";
import { cn } from "@/lib/utils";

export interface CodeEditorProps {
  id: string;
  /** Id of the visible label; CodeMirror's editable element is a div, which `<label for>` cannot target. */
  labelId?: string;
  language: CodeLanguage;
  value: string;
  onChange: (value: string) => void;
  rows?: number;
  placeholder?: string;
  className?: string;
}

/**
 * Plain textarea with the editor's props: shown while the CodeMirror chunk
 * loads, and kept if it fails to load.
 */
export function PlainCodeEditor({ id, value, onChange, rows = 8, placeholder, className }: CodeEditorProps) {
  return (
    <Textarea
      id={id}
      rows={rows}
      spellCheck={false}
      className={cn("font-mono text-xs md:text-xs", className)}
      value={value}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

const CodeMirrorEditor = lazy(() => importOr(() => import("./code-editor-impl"), PlainCodeEditor));

/** Syntax-highlighting editor; CodeMirror is fetched on first use, not with the app. */
export function CodeEditor(props: CodeEditorProps) {
  return (
    <Suspense fallback={<PlainCodeEditor {...props} />}>
      <CodeMirrorEditor {...props} />
    </Suspense>
  );
}
