import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { JsonInputField } from "./json-input-field";
import type { JsonFieldState } from "@/lib/json-field";

const value = (mode: JsonFieldState["mode"], text = ""): JsonFieldState => ({ kind: "json", mode, text });

describe("JsonInputField", () => {
  it("shows a monospace editor and an inline error for invalid JSON", () => {
    render(<JsonInputField id="i" fieldKey="cfg" field={{ type: "json" }} value={value("value", "{oops")} onChange={() => {}} />);
    expect(screen.getByLabelText("cfg").className).toContain("font-mono");
    expect(screen.getByRole("alert").textContent).toMatch(/^Invalid JSON/);
  });

  it("edits go through the labelled code editor once it loads", async () => {
    const onChange = vi.fn();
    render(<JsonInputField id="i" fieldKey="cfg" field={{ type: "json" }} value={value("value", "[1]")} onChange={onChange} />);
    const content = await waitFor(() => {
      const el = document.querySelector<HTMLElement>(".cm-content");
      expect(el).not.toBeNull();
      return el!;
    });
    expect(screen.getByRole("textbox", { name: "cfg" })).toBe(content);
    const view = EditorView.findFromDOM(content)!;
    view.dispatch({ changes: { from: 2, insert: ", 2" } });
    expect(onChange).toHaveBeenLastCalledWith({ kind: "json", mode: "value", text: "[1, 2]" });
  });

  it("highlights the read-only default", () => {
    render(<JsonInputField id="i" fieldKey="cfg" field={{ type: "json", default: { a: 1 } }} value={value("default")} onChange={() => {}} />);
    expect(screen.getByTestId("json-default-cfg").querySelector(".tok-propertyName")?.textContent).toBe('"a"');
  });

  it("shows a templated default read-only; Override opens an empty editor", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField
        id="i"
        fieldKey="cfg"
        field={{ type: "json", default: { host: "{{ secret.H }}" } }}
        value={value("default")}
        onChange={onChange}
      />,
    );
    expect(screen.getByTestId("json-default-cfg").textContent).toContain("{{ secret.H }}");
    fireEvent.click(screen.getByRole("button", { name: "Override" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "value", text: "" });
  });

  it("offers Use previous value on a re-run and replays a masked source", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField
        id="i"
        fieldKey="cfg"
        field={{ type: "json" }}
        value={value("value", "{}")}
        onChange={onChange}
        replaySource={{ value: { t: "••••••" } }}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Use previous value" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "replay", text: "" });
  });

  it("replay mode shows the source value; Edit prefills it when unmasked", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField
        id="i"
        fieldKey="cfg"
        field={{ type: "json", default: { a: 1 } }}
        value={value("replay")}
        onChange={onChange}
        replaySource={{ value: JSON.parse('{"n": 9007199254740993}') }}
      />,
    );
    expect(screen.getByTestId("json-replay-cfg").textContent).toContain("n");
    expect(screen.getByTestId("json-replay-note-cfg").textContent).toContain("reused exactly");
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "value", text: JSON.stringify({ n: 9007199254740992 }, null, 2) });
    fireEvent.click(screen.getByRole("button", { name: "Use default" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "default", text: "" });
  });

  it("Edit starts empty when the source is masked", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField id="i" fieldKey="cfg" field={{ type: "json" }} value={value("replay")} onChange={onChange} replaySource={{ value: { t: "••••••" } }} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "value", text: "" });
  });

  it("Use previous value replays even for an unmasked source", () => {
    const onChange = vi.fn();
    render(
      <JsonInputField id="i" fieldKey="cfg" field={{ type: "json" }} value={value("value", "{}")} onChange={onChange} replaySource={{ value: { a: 1 } }} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Use previous value" }));
    expect(onChange).toHaveBeenCalledWith({ kind: "json", mode: "replay", text: "" });
  });
});
