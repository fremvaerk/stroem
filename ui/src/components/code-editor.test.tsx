import { createRef, Fragment, StrictMode } from "react";
import { render, screen, waitFor } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type * as CodeEditorModule from "./code-editor";

// `React.lazy` caches the loaded chunk per module instance; a fresh module per
// test makes every test start from the loading fallback.
let mod: typeof CodeEditorModule;
beforeEach(async () => {
  vi.resetModules();
  mod = await import("./code-editor");
});

function setup(value: string, onChange = vi.fn(), { strict = false } = {}) {
  const Wrapper = strict ? StrictMode : Fragment;
  const ui = (v: string, labelId = "ed-label") => (
    <Wrapper>
      <label id="ed-label" htmlFor="ed">
        cfg
      </label>
      <label id="other-label">other</label>
      <mod.CodeEditor id="ed" labelId={labelId} language="json" value={v} onChange={onChange} />
    </Wrapper>
  );
  const utils = render(ui(value));
  return {
    ...utils,
    onChange,
    rerenderWith: (v: string, labelId?: string) => utils.rerender(ui(v, labelId)),
  };
}

async function editorView(): Promise<EditorView> {
  const content = await waitFor(() => {
    const el = document.querySelector<HTMLElement>(".cm-content");
    expect(el).not.toBeNull();
    return el!;
  });
  return EditorView.findFromDOM(content)!;
}

describe("CodeEditor", () => {
  it("renders a usable plain textarea until the editor chunk loads", () => {
    setup('{"a": 1}');
    const textarea = screen.getByLabelText("cfg");
    expect(textarea.tagName).toBe("TEXTAREA");
    expect((textarea as HTMLTextAreaElement).value).toBe('{"a": 1}');
  });

  it("replaces the fallback with a labelled, highlighted CodeMirror editor", async () => {
    setup('{"a": 1}');
    const view = await editorView();
    expect(screen.queryByRole("textbox", { name: "cfg" })).toBe(view.contentDOM);
    expect(view.contentDOM.textContent).toBe('{"a": 1}');
    await waitFor(() =>
      expect(view.contentDOM.querySelector(".tok-propertyName")?.textContent).toBe('"a"'),
    );
  });

  it("reports edits through onChange", async () => {
    const { onChange } = setup("[1]");
    const view = await editorView();
    view.dispatch({ changes: { from: 2, insert: ", 2" } });
    expect(onChange).toHaveBeenLastCalledWith("[1, 2]");
  });

  it("shows a value set from outside without echoing it back", async () => {
    const { onChange, rerenderWith } = setup("[1]");
    const view = await editorView();
    rerenderWith('{"b": 2}');
    expect(view.contentDOM.textContent).toBe('{"b": 2}');
    expect(onChange).not.toHaveBeenCalled();
  });

  it.each([
    ["", false],
    [" under StrictMode (development effect replay destroys the first view)", true],
  ])("moves focus and selection from the loading textarea into the editor%s", async (_, strict) => {
    setup("[1, 2, 3]", vi.fn(), { strict });
    const textarea = screen.getByLabelText("cfg") as HTMLTextAreaElement;
    textarea.focus();
    textarea.setSelectionRange(1, 3, "backward");
    const view = await editorView();
    expect(view.hasFocus).toBe(true);
    expect(view.state.selection.main.anchor).toBe(3);
    expect(view.state.selection.main.head).toBe(1);
  });

  it.each([false, true])("does not grab focus when the loading textarea was not focused (strict: %s)", async (strict) => {
    setup("[1]", vi.fn(), { strict });
    const view = await editorView();
    expect(view.hasFocus).toBe(false);
  });

  it("updates its configuration in place, keeping the editing session", async () => {
    const { rerenderWith } = setup("[1]");
    const view = await editorView();
    view.dispatch({ changes: { from: 2, insert: ", 2" }, selection: { anchor: 4 } });
    rerenderWith("[1, 2]", "other-label");
    expect(EditorView.findFromDOM(document.querySelector(".cm-content")!)).toBe(view);
    expect(screen.getByRole("textbox", { name: "other" })).toBe(view.contentDOM);
    expect(view.state.selection.main.head).toBe(4);
  });
});

describe("PlainCodeEditor", () => {
  it.each([false, true])("takes over a pending focus handoff when it mounts (strict: %s)", (strict) => {
    const Wrapper = strict ? StrictMode : Fragment;
    const handoffRef = createRef<CodeEditorModule.FocusHandoff | null>() as {
      current: CodeEditorModule.FocusHandoff | null;
    };
    handoffRef.current = { anchor: 3, head: 1 };
    render(
      <Wrapper>
        <mod.PlainCodeEditor id="p" language="json" value="[1, 2]" onChange={() => {}} handoffRef={handoffRef} />
      </Wrapper>,
    );
    const textarea = document.getElementById("p") as HTMLTextAreaElement;
    expect(document.activeElement).toBe(textarea);
    expect([textarea.selectionStart, textarea.selectionEnd, textarea.selectionDirection]).toEqual([1, 3, "backward"]);
    expect(handoffRef.current).toBeNull();
  });
});
