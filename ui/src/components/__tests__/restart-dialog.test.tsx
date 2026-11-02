import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { RestartDialog } from "../restart-dialog";
import type { RestartPlanResponse } from "@/lib/api";

function makePlan(overrides: Partial<RestartPlanResponse> = {}): RestartPlanResponse {
  return {
    restart_steps: ["b", "c"],
    carried_over: ["a"],
    carried_failed: ["z"],
    carried_failed_tolerated: [],
    ...overrides,
  };
}

function renderDialog(
  plan: RestartPlanResponse,
  handlers: { onConfirm?: () => void; onCancel?: () => void } = {},
) {
  return render(
    <RestartDialog
      open
      plan={plan}
      taskName="line"
      target={{ kind: "step", step: "b" }}
      onConfirm={handlers.onConfirm ?? (() => {})}
      onCancel={handlers.onCancel ?? (() => {})}
    />,
  );
}

describe("RestartDialog", () => {
  it("names the task and the step it restarts from", () => {
    renderDialog(makePlan());

    expect(document.body.textContent).toContain("Restart line from b?");
  });

  it("summarises the rerun set and the carried-over count", () => {
    renderDialog(makePlan());

    const text = document.body.textContent ?? "";
    expect(text).toContain("Reruns 2 step(s)");
    expect(text).toContain("b, c");
    expect(text).toContain("1 step(s) are carried over");
  });

  it("warns about carried-over failures that are not tolerated", () => {
    renderDialog(makePlan({ carried_failed: ["z"] }));

    const warning = screen.getByTestId("restart-failure-warning");
    expect(warning).toBeInTheDocument();
    expect(warning.textContent).toContain("z");
    expect(warning.textContent).toContain("1 carried-over step(s) ended failed");
  });

  it("omits the warning when no carried-over step failed", () => {
    renderDialog(makePlan({ carried_failed: [] }));

    expect(screen.queryByTestId("restart-failure-warning")).not.toBeInTheDocument();
  });

  it("titles a restart of failed steps and lists its start steps", () => {
    render(
      <RestartDialog
        open
        plan={makePlan({
          start_steps: ["b", "c"],
          restart_steps: ["c", "b", "d"],
          carried_failed: [],
        })}
        taskName="line"
        target={{ kind: "failed" }}
        onConfirm={() => {}}
        onCancel={() => {}}
      />,
    );

    const text = document.body.textContent ?? "";
    expect(text).toContain("Restart the failed steps of line?");
    expect(text).toContain("Starts at 2 failed or cancelled step(s): b, c.");
    expect(text).toContain("Reruns 3 step(s): b, c, d.");
  });

  it("notes carried-over failures that continue_on_failure tolerates", () => {
    renderDialog(makePlan({ carried_failed: [], carried_failed_tolerated: ["y", "x"] }));

    const note = screen.getByTestId("restart-tolerated-note");
    expect(note.textContent).toContain(
      "2 carried-over step(s) ended failed but continue_on_failure lets the job pass (x, y). They are not rerun.",
    );
    expect(note.textContent).toContain("Use Restart failed steps to rerun them.");
    expect(screen.queryByTestId("restart-failure-warning")).not.toBeInTheDocument();
  });

  it("omits the tolerated note when there is none or an older server omits it", () => {
    renderDialog(makePlan({ carried_failed_tolerated: [] }));
    expect(screen.queryByTestId("restart-tolerated-note")).not.toBeInTheDocument();
    const { carried_failed_tolerated: _omit, ...older } = makePlan();
    void _omit;
    renderDialog(older);
    expect(screen.queryByTestId("restart-tolerated-note")).not.toBeInTheDocument();
  });

  it("calls onConfirm when the confirm button is clicked", () => {
    const onConfirm = vi.fn();
    renderDialog(makePlan(), { onConfirm });

    fireEvent.click(screen.getByRole("button", { name: /^restart$/i }));

    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("calls onCancel when the cancel button is clicked", () => {
    const onCancel = vi.fn();
    renderDialog(makePlan(), { onCancel });

    fireEvent.click(screen.getByRole("button", { name: /cancel/i }));

    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("disables both actions while a restart is in flight", () => {
    render(
      <RestartDialog
        open
        busy
        plan={makePlan()}
        taskName="line"
        target={{ kind: "step", step: "b" }}
        onConfirm={() => {}}
        onCancel={() => {}}
      />,
    );

    expect(screen.getByRole("button", { name: /restarting/i })).toBeDisabled();
    expect(screen.getByRole("button", { name: /cancel/i })).toBeDisabled();
  });

  it("renders nothing when there is no plan", () => {
    render(
      <RestartDialog
        open
        plan={null}
        taskName="line"
        target={{ kind: "step", step: "b" }}
        onConfirm={() => {}}
        onCancel={() => {}}
      />,
    );

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});
