import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { ApprovalCard } from "./approval-card";
import type { JobStep } from "@/lib/types";

// approveStep is called from button handlers — stub it so the test stays UI-only.
vi.mock("@/lib/api", () => ({
  approveStep: vi.fn().mockResolvedValue({}),
}));

function makeStep(approvalFields: Record<string, unknown>): JobStep {
  return {
    step_name: "review",
    action_name: "approve",
    action_type: "approval",
    action_image: null,
    runner: "none",
    input: null,
    output: null,
    status: "suspended",
    worker_id: null,
    started_at: null,
    completed_at: null,
    suspended_at: "2026-05-01T00:00:00Z",
    error_message: null,
    when_condition: null,
    depends_on: [],
    for_each_expr: null,
    loop_source: null,
    loop_index: null,
    loop_total: null,
    retry_attempt: 0,
    max_retries: null,
    retry_history: [],
    retry_at: null,
    approval_message: "Pick deployment targets",
    approval_fields: approvalFields,
  };
}

describe("ApprovalCard (approval gate)", () => {
  it("renders MultiSelectField for a multiple:true field", () => {
    const step = makeStep({
      environments: {
        type: "string",
        options: ["dev", "staging", "prod"],
        multiple: true,
        required: true,
      },
    });

    render(<ApprovalCard jobId="job-1" step={step} onAction={() => {}} />);

    // Trigger should be a combobox (multi-select trigger), with aria-haspopup=listbox
    const trigger = screen.getByRole("combobox", { name: /environments/i });
    expect(trigger).toHaveAttribute("aria-haspopup", "listbox");
  });

  it("toggling options updates approvalInput state (chips appear in trigger)", () => {
    const step = makeStep({
      environments: {
        type: "string",
        options: ["dev", "staging", "prod"],
        multiple: true,
      },
    });

    render(<ApprovalCard jobId="job-1" step={step} onAction={() => {}} />);

    const trigger = screen.getByRole("combobox", { name: /environments/i });
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("option", { name: /dev/ }));
    fireEvent.click(screen.getByRole("option", { name: /staging/ }));
    fireEvent.keyDown(document, { key: "Escape" });

    expect(within(trigger).getByText("dev")).toBeInTheDocument();
    expect(within(trigger).getByText("staging")).toBeInTheDocument();
  });

  it("falls back to ComboboxField when multiple is omitted", () => {
    const step = makeStep({
      env: {
        type: "string",
        options: ["dev", "staging"],
      },
    });

    render(<ApprovalCard jobId="job-1" step={step} onAction={() => {}} />);

    // Single-select trigger has aria-haspopup unset (ComboboxField doesn't set it).
    const trigger = screen.getByLabelText(/env/);
    expect(trigger).not.toHaveAttribute("aria-haspopup", "listbox");
  });
});
