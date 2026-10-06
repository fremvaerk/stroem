import { afterEach, beforeEach, describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { InputFieldRow } from "./input-field-row";
import type { InputField } from "@/lib/types";

function renderRow(fieldKey: string, field: InputField, value: unknown = "") {
  return render(
    <InputFieldRow
      fieldKey={fieldKey}
      field={field}
      value={value}
      onChange={() => {}}
    />,
  );
}

describe("InputFieldRow placeholders", () => {
  const description = "Git ref to deploy, e.g. a tag or branch";

  it("text input uses the field key as placeholder, description only as helper text", () => {
    renderRow("ref", { type: "string", description });
    const input = screen.getByLabelText("ref") as HTMLInputElement;
    expect(input.placeholder).toBe("ref");
    expect(screen.getAllByText(description)).toHaveLength(1);
  });

  it("secret input does not leak the description into the placeholder", () => {
    renderRow("token", { type: "string", secret: true, description });
    const input = screen.getByLabelText("token") as HTMLInputElement;
    expect(input.type).toBe("password");
    expect(input.placeholder).toBe("token");
  });

  it("multiline text input uses the field key as placeholder", () => {
    renderRow("notes", { type: "text", description });
    const textarea = screen.getByLabelText("notes") as HTMLTextAreaElement;
    expect(textarea.placeholder).toBe("notes");
    expect(screen.getAllByText(description)).toHaveLength(1);
  });

  it("option fields show 'Select <label>' rather than the description", () => {
    renderRow("env", {
      type: "string",
      name: "Environment",
      description,
      options: ["staging", "prod"],
    });
    expect(screen.getByText("Select environment")).toBeTruthy();
    expect(screen.getAllByText(description)).toHaveLength(1);
  });

  it("falls back to the key when no description is set", () => {
    renderRow("ref", { type: "string" });
    const input = screen.getByLabelText("ref") as HTMLInputElement;
    expect(input.placeholder).toBe("ref");
  });
});

describe("InputFieldRow date picker", () => {
  // DayPicker opens on today's month (it does not follow `selected`), so pin
  // the clock to the month under test. Only `Date` is faked: Radix popovers
  // rely on real timers.
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date(2026, 2, 10, 12, 0));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("focuses the selected day on open and emits yyyy-MM-dd on pick", () => {
    const onChange = vi.fn();
    render(
      <InputFieldRow
        fieldKey="day"
        field={{ type: "date" }}
        value="2026-03-15"
        onChange={onChange}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /March 15, 2026/ }));

    const grid = screen.getByRole("grid");
    expect(grid).toHaveAccessibleName(/March 2026/);
    // `autoFocus` (v10's replacement for `initialFocus`) focuses the selected day.
    expect(document.activeElement).toHaveTextContent("15");

    fireEvent.click(within(grid).getByRole("button", { name: /March 20/ }));
    expect(onChange).toHaveBeenCalledWith("2026-03-20");
  });

  it("datetime field keeps the time part when a day is picked", () => {
    const onChange = vi.fn();
    render(
      <InputFieldRow
        fieldKey="at"
        field={{ type: "datetime" }}
        value="2026-03-15T09:30"
        onChange={onChange}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /March 15, 2026/ }));
    fireEvent.click(
      within(screen.getByRole("grid")).getByRole("button", { name: /March 20/ }),
    );
    expect(onChange).toHaveBeenCalledWith("2026-03-20T09:30");
  });
});
