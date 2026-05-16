import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { MultiSelectField } from "./multi-select-field";

function Harness({
  options,
  initial = [],
  onChange,
  allowCustom = false,
  required = false,
}: {
  options: string[];
  initial?: string[];
  onChange?: (v: string[]) => void;
  allowCustom?: boolean;
  required?: boolean;
}) {
  const [value, setValue] = useState<string[]>(initial);
  return (
    <MultiSelectField
      id="envs"
      label="Environments"
      options={options}
      value={value}
      onChange={(v) => {
        setValue(v);
        onChange?.(v);
      }}
      placeholder="Pick envs"
      required={required}
      allowCustom={allowCustom}
    />
  );
}

function getTrigger() {
  return screen.getByRole("combobox", { name: /Environments/i });
}

function openPopover() {
  fireEvent.click(getTrigger());
}

describe("MultiSelectField", () => {
  it("shows placeholder when nothing selected", () => {
    render(<Harness options={["dev", "staging", "prod"]} />);
    expect(screen.getByText("Pick envs")).toBeInTheDocument();
  });

  it("toggles options on click and emits an array", () => {
    const onChange = vi.fn();
    render(
      <Harness options={["dev", "staging", "prod"]} onChange={onChange} />,
    );
    openPopover();
    fireEvent.click(screen.getByRole("option", { name: /dev/ }));
    expect(onChange).toHaveBeenLastCalledWith(["dev"]);
    fireEvent.click(screen.getByRole("option", { name: /staging/ }));
    expect(onChange).toHaveBeenLastCalledWith(["dev", "staging"]);
    // Toggle off
    fireEvent.click(screen.getByRole("option", { name: /dev/ }));
    expect(onChange).toHaveBeenLastCalledWith(["staging"]);
  });

  it("renders a chip per selected value (no collapsing)", () => {
    render(
      <Harness
        options={["dev", "staging", "prod", "sandbox"]}
        initial={["dev", "staging", "prod", "sandbox"]}
      />,
    );
    const trigger = getTrigger();
    // Every selected value should be visible as a chip
    expect(within(trigger).getByText("dev")).toBeInTheDocument();
    expect(within(trigger).getByText("staging")).toBeInTheDocument();
    expect(within(trigger).getByText("prod")).toBeInTheDocument();
    expect(within(trigger).getByText("sandbox")).toBeInTheDocument();
    // No "N selected" string
    expect(trigger).not.toHaveTextContent(/\d+ selected/);
  });

  it("stays open across multiple selections (popover not closed)", () => {
    render(<Harness options={["dev", "staging", "prod"]} />);
    openPopover();
    fireEvent.click(screen.getByRole("option", { name: /dev/ }));
    expect(
      screen.getByPlaceholderText("Search or type a value..."),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole("option", { name: /staging/ }));
    expect(
      screen.getByPlaceholderText("Search or type a value..."),
    ).toBeInTheDocument();
  });

  it("adds a typed value when allowCustom is true and no exact match", () => {
    const onChange = vi.fn();
    render(
      <Harness options={["dev", "staging"]} allowCustom onChange={onChange} />,
    );
    openPopover();
    const search = screen.getByPlaceholderText("Search or type a value...");
    fireEvent.change(search, { target: { value: "sandbox" } });
    fireEvent.click(screen.getByText(/Add/));
    expect(onChange).toHaveBeenLastCalledWith(["sandbox"]);
  });

  it("shows a custom-added item in the list so it can be removed", () => {
    // Regression: previously, a value added via allow_custom was invisible in
    // the list (only `options` were rendered), so users couldn't deselect it.
    render(
      <Harness
        options={["dev", "staging"]}
        initial={["dev", "hotfix"]}
        allowCustom
      />,
    );
    openPopover();
    // "hotfix" appears in the popover list with a checked Checkbox
    const hotfixCheckbox = screen.getByRole("checkbox", { name: "hotfix" });
    expect(hotfixCheckbox).toHaveAttribute("data-state", "checked");
  });

  it("does not offer custom add when value matches an existing item (option or custom)", () => {
    render(
      <Harness
        options={["dev", "staging"]}
        initial={["sandbox"]}
        allowCustom
      />,
    );
    openPopover();
    const search = screen.getByPlaceholderText("Search or type a value...");
    // matches an option
    fireEvent.change(search, { target: { value: "dev" } });
    expect(screen.queryByText(/Add /)).toBeNull();
    // matches an existing custom value
    fireEvent.change(search, { target: { value: "sandbox" } });
    expect(screen.queryByText(/Add /)).toBeNull();
  });

  it("renders the required asterisk", () => {
    const { container } = render(<Harness options={["a", "b"]} required />);
    expect(container.querySelector(".text-destructive")).toBeInTheDocument();
  });

  it("shows prefilled values on mount including custom entries", () => {
    render(
      <Harness
        options={["dev", "staging", "prod"]}
        initial={["dev", "sandbox"]}
        allowCustom
      />,
    );
    const trigger = getTrigger();
    expect(within(trigger).getByText("dev")).toBeInTheDocument();
    expect(within(trigger).getByText("sandbox")).toBeInTheDocument();
  });

  it("returns to placeholder when all selections are removed", () => {
    const onChange = vi.fn();
    render(
      <Harness
        options={["dev", "staging"]}
        initial={["dev"]}
        onChange={onChange}
      />,
    );
    openPopover();
    fireEvent.click(screen.getByRole("option", { name: /dev/ }));
    expect(onChange).toHaveBeenLastCalledWith([]);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.getByText("Pick envs")).toBeInTheDocument();
  });

  it("closes the popover on Escape", () => {
    render(<Harness options={["dev", "staging"]} />);
    openPopover();
    expect(
      screen.getByPlaceholderText("Search or type a value..."),
    ).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(
      screen.queryByPlaceholderText("Search or type a value..."),
    ).not.toBeInTheDocument();
  });

  it("aria-label reflects the full selection", () => {
    render(
      <Harness options={["a", "b", "c", "d"]} initial={["a", "b", "c", "d"]} />,
    );
    expect(
      screen.getByRole("combobox", { name: "Environments: a, b, c, d" }),
    ).toBeInTheDocument();
  });

  it("X button on a chip removes that value without opening the popover", () => {
    const onChange = vi.fn();
    render(
      <Harness
        options={["dev", "staging", "prod"]}
        initial={["dev", "staging"]}
        onChange={onChange}
      />,
    );
    // Popover starts closed
    expect(
      screen.queryByPlaceholderText("Search or type a value..."),
    ).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Remove dev" }));

    expect(onChange).toHaveBeenLastCalledWith(["staging"]);
    // Popover did NOT open as a side effect
    expect(
      screen.queryByPlaceholderText("Search or type a value..."),
    ).not.toBeInTheDocument();
  });

  it("exposes selection state via the Checkbox data-state", () => {
    render(<Harness options={["dev", "staging"]} initial={["dev"]} />);
    openPopover();
    expect(screen.getByRole("checkbox", { name: "dev" })).toHaveAttribute(
      "data-state",
      "checked",
    );
    expect(screen.getByRole("checkbox", { name: "staging" })).toHaveAttribute(
      "data-state",
      "unchecked",
    );
  });
});
