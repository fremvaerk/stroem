import { render, screen, waitFor } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { describe, expect, it, vi } from "vitest";
import { CodeEditor } from "./code-editor";

function setup(value: string, onChange = vi.fn()) {
  const ui = (v: string) => (
    <>
      <label id="ed-label" htmlFor="ed">
        cfg
      </label>
      <CodeEditor id="ed" labelId="ed-label" language="json" value={v} onChange={onChange} />
    </>
  );
  const utils = render(ui(value));
  return { ...utils, onChange, rerenderWith: (v: string) => utils.rerender(ui(v)) };
}

async function editorContent(): Promise<HTMLElement> {
  return waitFor(() => {
    const el = document.querySelector<HTMLElement>(".cm-content");
    expect(el).not.toBeNull();
    return el!;
  });
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
    const content = await editorContent();
    expect(screen.queryByRole("textbox", { name: "cfg" })).toBe(content);
    expect(content.textContent).toBe('{"a": 1}');
    await waitFor(() => expect(content.querySelector(".tok-propertyName")?.textContent).toBe('"a"'));
  });

  it("reports edits through onChange", async () => {
    const { onChange } = setup("[1]");
    const view = EditorView.findFromDOM(await editorContent())!;
    view.dispatch({ changes: { from: 2, insert: ", 2" } });
    expect(onChange).toHaveBeenLastCalledWith("[1, 2]");
  });

  it("shows a value set from outside without echoing it back", async () => {
    const { onChange, rerenderWith } = setup("[1]");
    const content = await editorContent();
    rerenderWith('{"b": 2}');
    expect(content.textContent).toBe('{"b": 2}');
    expect(onChange).not.toHaveBeenCalled();
  });
});
