import { afterEach, beforeEach, describe, it, expect, vi } from "vitest";
import { useState } from "react";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
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

// The form owns the value; these tests need a parent that feeds `onChange`
// back in, as task-detail does, or typing is reset on the next render.
function StatefulRow({
  fieldKey,
  field,
  initial,
  onChange,
}: {
  fieldKey: string;
  field: InputField;
  initial: string;
  onChange: (v: unknown) => void;
}) {
  const [value, setValue] = useState<unknown>(initial);
  return (
    <InputFieldRow
      fieldKey={fieldKey}
      field={field}
      value={value}
      onChange={(v) => {
        setValue(v);
        onChange(v);
      }}
    />
  );
}

describe("InputFieldRow date field", () => {
  // DayPicker defaults to today's month, so pin the clock. Only `Date` is
  // faked: Radix popovers rely on real timers.
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date(2026, 2, 10, 12, 0));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function renderDate(initial: string) {
    const onChange = vi.fn();
    render(
      <StatefulRow
        fieldKey="day"
        field={{ type: "date" }}
        initial={initial}
        onChange={onChange}
      />,
    );
    const input = screen.getByLabelText("day") as HTMLInputElement;
    return { onChange, input };
  }

  function openCalendar(label = "day") {
    fireEvent.click(
      screen.getByRole("button", { name: `Open calendar for ${label}` }),
    );
    return screen.getByRole("grid");
  }

  it("shows the value as ISO in a labelled text box", () => {
    const { input } = renderDate("2026-03-15");
    expect(input.value).toBe("2026-03-15");
    expect(input.placeholder).toBe("YYYY-MM-DD");
  });

  it("typing a date emits it as YYYY-MM-DD", () => {
    const { input, onChange } = renderDate("");
    fireEvent.change(input, { target: { value: "2019-05-03" } });
    expect(onChange).toHaveBeenLastCalledWith("2019-05-03");
    expect(input.validity.valid).toBe(true);
  });

  it("invalid text emits empty, blocks the form, and is flagged on blur", () => {
    const { input, onChange } = renderDate("2026-03-15");
    act(() => input.focus());
    fireEvent.change(input, { target: { value: "2026-02-31" } });

    expect(onChange).toHaveBeenLastCalledWith("");
    expect(input.value).toBe("2026-02-31");
    expect(input.validity.valid).toBe(false);
    // Not while typing...
    expect(input).not.toHaveAttribute("aria-invalid");

    act(() => input.blur());
    // ...only once the user leaves the field.
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAccessibleDescription(/YYYY-MM-DD/);
  });

  it("short-hand text is emitted canonical and tidied on blur", () => {
    const { input, onChange } = renderDate("");
    act(() => input.focus());
    fireEvent.change(input, { target: { value: "2026-3-5" } });
    expect(onChange).toHaveBeenLastCalledWith("2026-03-05");
    // Left as typed while editing...
    expect(input.value).toBe("2026-3-5");

    act(() => input.blur());
    expect(input.value).toBe("2026-03-05");
  });

  it("clearing the text emits empty and is valid", () => {
    const { input, onChange } = renderDate("2026-03-15");
    fireEvent.change(input, { target: { value: "" } });
    expect(onChange).toHaveBeenLastCalledWith("");
    expect(input.validity.valid).toBe(true);
    expect(input).not.toHaveAttribute("aria-invalid");
  });

  it("the calendar opens on the value's month, not today's", () => {
    renderDate("2019-05-03");
    const grid = openCalendar();
    expect(grid).toHaveAccessibleName(/May 2019/);
    // `autoFocus` focuses the selected day.
    expect(document.activeElement).toHaveTextContent("3");
  });

  it("the calendar follows a typed date, including future years", () => {
    const { input } = renderDate("");
    fireEvent.change(input, { target: { value: "2031-07-20" } });
    expect(openCalendar()).toHaveAccessibleName(/July 2031/);
  });

  it("picking a day updates the text and closes the calendar", () => {
    const { input, onChange } = renderDate("2026-03-15");
    const grid = openCalendar();
    fireEvent.click(within(grid).getByRole("button", { name: /March 20/ }));

    expect(onChange).toHaveBeenLastCalledWith("2026-03-20");
    expect(input.value).toBe("2026-03-20");
    expect(screen.queryByRole("grid")).toBeNull();
  });

  it("the year dropdown jumps years and spans 100 back to 20 ahead", () => {
    renderDate("2026-03-15");
    openCalendar();
    const years = screen.getByRole("combobox", { name: "Choose the Year" });
    const options = within(years)
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(options[0]).toBe("1926");
    expect(options[options.length - 1]).toBe("2046");

    fireEvent.change(years, { target: { value: "2010" } });
    expect(screen.getByRole("grid")).toHaveAccessibleName(/March 2010/);
  });

  it("the year range widens to include a value outside it", () => {
    renderDate("1890-06-01");
    openCalendar();
    const years = screen.getByRole("combobox", { name: "Choose the Year" });
    expect(within(years).getAllByRole("option")[0]).toHaveTextContent("1890");
  });

  it("Today picks today's date", () => {
    const { onChange } = renderDate("");
    openCalendar();
    fireEvent.click(screen.getByRole("button", { name: "Today" }));
    expect(onChange).toHaveBeenLastCalledWith("2026-03-10");
  });

  it("a new value from the parent (re-run prefill) replaces the text", () => {
    const props = { fieldKey: "day", field: { type: "date" }, onChange: () => {} };
    const { rerender } = render(<InputFieldRow {...props} value="2026-03-15" />);
    rerender(<InputFieldRow {...props} value="2027-01-01" />);
    expect((screen.getByLabelText("day") as HTMLInputElement).value).toBe(
      "2027-01-01",
    );
  });
});

describe("InputFieldRow datetime field", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date(2026, 2, 10, 12, 0));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function renderDateTime(initial: string) {
    const onChange = vi.fn();
    render(
      <StatefulRow
        fieldKey="at"
        field={{ type: "datetime" }}
        initial={initial}
        onChange={onChange}
      />,
    );
    return {
      onChange,
      date: screen.getByLabelText("at") as HTMLInputElement,
      time: screen.getByLabelText("at time") as HTMLInputElement,
    };
  }

  it("splits the value into the date and time boxes", () => {
    const { date, time } = renderDateTime("2026-03-15T09:30");
    expect(date.value).toBe("2026-03-15");
    expect(time.value).toBe("09:30");
  });

  it("picking a day keeps the time", () => {
    const { onChange } = renderDateTime("2026-03-15T09:30");
    fireEvent.click(
      screen.getByRole("button", { name: "Open calendar for at" }),
    );
    fireEvent.click(
      within(screen.getByRole("grid")).getByRole("button", { name: /March 20/ }),
    );
    expect(onChange).toHaveBeenLastCalledWith("2026-03-20T09:30");
  });

  it("retyping the date keeps the time through the invalid middle", () => {
    const { date, time, onChange } = renderDateTime("2026-03-15T09:30");
    fireEvent.change(date, { target: { value: "2026-03-" } });
    expect(onChange).toHaveBeenLastCalledWith("");
    expect(time.value).toBe("09:30");

    fireEvent.change(date, { target: { value: "2026-03-16" } });
    expect(onChange).toHaveBeenLastCalledWith("2026-03-16T09:30");
  });

  it("a time without a date blocks the form instead of being dropped", () => {
    const { date, time, onChange } = renderDateTime("");
    expect(date.validity.valid).toBe(true);

    fireEvent.change(time, { target: { value: "09:30" } });
    expect(onChange).toHaveBeenLastCalledWith("");
    expect(date.validity.valueMissing).toBe(true);
  });

  it("changing the time keeps the date", () => {
    const { time, onChange } = renderDateTime("2026-03-15T09:30");
    fireEvent.change(time, { target: { value: "17:45" } });
    expect(onChange).toHaveBeenLastCalledWith("2026-03-15T17:45");
  });
});
