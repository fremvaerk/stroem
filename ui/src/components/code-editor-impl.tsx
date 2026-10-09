import { useEffect, useEffectEvent, useRef, useState } from "react";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { json } from "@codemirror/lang-json";
import { bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { Annotation, Compartment, EditorSelection, EditorState, type Extension } from "@codemirror/state";
import { EditorView, keymap, placeholder as placeholderText } from "@codemirror/view";
import { classHighlighter } from "@lezer/highlight";
import type { CodeLanguage } from "@/lib/code-language";
import { cn } from "@/lib/utils";
import type { EditorImplProps } from "./code-editor";

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

type ConfigProps = Pick<EditorImplProps, "id" | "labelId" | "language" | "placeholder"> & { rows: number };

/** The prop-driven extensions, swapped in place through a compartment when a prop changes. */
function configExtensions({ id, labelId, language, placeholder, rows }: ConfigProps): Extension {
  const attrs: Record<string, string> = { id };
  if (labelId) attrs["aria-labelledby"] = labelId;
  return [
    languageSupport[language](),
    EditorView.contentAttributes.of(attrs),
    placeholder ? placeholderText(placeholder) : [],
    EditorView.theme({ ".cm-content": { minHeight: `${rows}lh` } }),
  ];
}

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
  handoffRef,
}: EditorImplProps) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const [config] = useState(() => new Compartment());
  const report = useEffectEvent((text: string) => onChange(text));
  const initialDoc = useEffectEvent(() => value);
  const initialConfig = useEffectEvent(() => configExtensions({ id, labelId, language, placeholder, rows }));
  const takeHandoff = useEffectEvent(() => {
    const pending = handoffRef?.current ?? null;
    if (handoffRef) handoffRef.current = null;
    return pending;
  });

  useEffect(() => {
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: initialDoc(),
        extensions: [
          history(),
          keymap.of([...defaultKeymap, ...historyKeymap]),
          indentOnInput(),
          bracketMatching(),
          syntaxHighlighting(classHighlighter),
          EditorView.lineWrapping,
          EditorState.tabSize.of(2),
          baseTheme,
          config.of(initialConfig()),
          EditorView.updateListener.of((u) => {
            if (u.docChanged && !u.transactions.some((t) => t.annotation(fromProps))) {
              report(u.state.doc.toString());
            }
          }),
        ],
      }),
    });
    viewRef.current = view;

    // The loading textarea had focus when it was swapped out: continue where it left off.
    const pending = takeHandoff();
    if (pending) {
      const len = view.state.doc.length;
      view.dispatch({ selection: EditorSelection.single(Math.min(pending.anchor, len), Math.min(pending.head, len)) });
      view.focus();
    }

    return () => {
      view.destroy();
      viewRef.current = null;
    };
  }, [config]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: config.reconfigure(configExtensions({ id, labelId, language, placeholder, rows })),
    });
  }, [config, id, labelId, language, placeholder, rows]);

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
