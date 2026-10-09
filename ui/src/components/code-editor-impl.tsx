import { useEffect, useEffectEvent, useRef } from "react";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { json } from "@codemirror/lang-json";
import { bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { Annotation, EditorState, type Extension } from "@codemirror/state";
import { EditorView, keymap, placeholder as placeholderText } from "@codemirror/view";
import { classHighlighter } from "@lezer/highlight";
import type { CodeLanguage } from "@/lib/code-language";
import { cn } from "@/lib/utils";
import type { CodeEditorProps } from "./code-editor";

const languageSupport: Record<CodeLanguage, () => Extension> = {
  json: () => json(),
};

/** Marks a change that came from the `value` prop, so it is not reported back through `onChange`. */
const fromProps = Annotation.define<boolean>();

// Layout only; colours come from the app's CSS variables and the `tok-*` rules in index.css.
const baseTheme = EditorView.theme({
  "&": { maxHeight: "24rem" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { overflow: "auto", fontFamily: "inherit", lineHeight: "inherit" },
  ".cm-content": { padding: "0.5rem 0", caretColor: "var(--foreground)" },
  ".cm-line": { padding: "0 0.75rem" },
  ".cm-placeholder": { color: "var(--muted-foreground)" },
});

/** CodeMirror 6 editor; loaded lazily through `CodeEditor`. */
export default function CodeEditorImpl({
  id,
  labelId,
  language,
  value,
  onChange,
  rows = 8,
  placeholder,
  className,
}: CodeEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const report = useEffectEvent((text: string) => onChange(text));
  const currentValue = useEffectEvent(() => value);

  useEffect(() => {
    const attrs: Record<string, string> = { id };
    if (labelId) attrs["aria-labelledby"] = labelId;
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: currentValue(),
        extensions: [
          history(),
          keymap.of([...defaultKeymap, ...historyKeymap]),
          indentOnInput(),
          bracketMatching(),
          languageSupport[language](),
          syntaxHighlighting(classHighlighter),
          EditorView.lineWrapping,
          EditorView.contentAttributes.of(attrs),
          EditorState.tabSize.of(2),
          placeholder ? placeholderText(placeholder) : [],
          baseTheme,
          EditorView.theme({ ".cm-content": { minHeight: `${rows}lh` } }),
          EditorView.updateListener.of((u) => {
            if (u.docChanged && !u.transactions.some((t) => t.annotation(fromProps))) {
              report(u.state.doc.toString());
            }
          }),
        ],
      }),
    });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
  }, [id, labelId, language, placeholder, rows]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const doc = view.state.doc.toString();
    if (doc !== value) {
      view.dispatch({ changes: { from: 0, to: doc.length, insert: value }, annotations: fromProps.of(true) });
    }
  }, [value]);

  return (
    <div
      ref={host}
      className={cn(
        "rounded-md border border-input bg-transparent font-mono text-xs shadow-sm focus-within:ring-1 focus-within:ring-ring",
        className,
      )}
    />
  );
}
