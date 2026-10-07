import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { JsonInputField } from "./json-input-field";
import type { JsonFieldState } from "@/lib/json-field";

const value = (mode: JsonFieldState["mode"], text = ""): JsonFieldState => ({ kind: "json", mode, text });

describe("JsonInputField", () => {
  it("shows a monospace editor and an inline error for invalid JSON", () => {
    render(<JsonInputField id="i" fieldKey="cfg" field={{ type: "json" }} value={value("value", "{oops")} onChange={() => {}} />);
    expect(screen.getByLabelText("cfg").className).toContain("font-mono");
    expect(screen.getByRole("alert").textContent).toMatch(/^Invalid JSON/);
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
});
