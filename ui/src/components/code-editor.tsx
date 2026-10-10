import { lazy, type RefObject, Suspense, useLayoutEffect, useRef } from "react";
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

/** Focus and selection of an editor that is being swapped out, for the one replacing it. */
export interface FocusHandoff {
  anchor: number;
  head: number;
}

export interface EditorImplProps extends CodeEditorProps {
  /** Owned by `CodeEditor`; set when a focused editor unmounts, taken by the next one to mount. */
  handoffRef?: RefObject<FocusHandoff | null>;
}

/**
 * Plain textarea with the editor's props: shown while the CodeMirror chunk
 * loads, and kept if it fails to load.
 */
export function PlainCodeEditor({
  id,
  value,
  onChange,
  rows = 8,
  placeholder,
  className,
  handoffRef,
}: EditorImplProps) {
  const ref = useRef<HTMLTextAreaElement>(null);

  // Layout effects: the cleanup runs before React removes the textarea, while
  // it still has focus, so a swap (Suspense resolving) can carry focus across.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || !handoffRef) return;
    const pending = handoffRef.current;
    if (pending) {
      handoffRef.current = null;
      el.focus();
      const [start, end] = [Math.min(pending.anchor, pending.head), Math.max(pending.anchor, pending.head)];
      el.setSelectionRange(start, end, pending.anchor > pending.head ? "backward" : "forward");
    }
    return () => {
      if (document.activeElement !== el) return;
      const backward = el.selectionDirection === "backward";
      handoffRef.current = {
        anchor: backward ? el.selectionEnd : el.selectionStart,
        head: backward ? el.selectionStart : el.selectionEnd,
      };
    };
  }, [handoffRef]);

  return (
    <Textarea
      ref={ref}
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
  const handoffRef = useRef<FocusHandoff | null>(null);
  return (
    <Suspense fallback={<PlainCodeEditor {...props} handoffRef={handoffRef} />}>
      <CodeMirrorEditor {...props} handoffRef={handoffRef} />
    </Suspense>
  );
}
