import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
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
