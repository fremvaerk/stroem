import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, within } from "@testing-library/react";
import { WorkflowDag } from "../workflow-dag";
import type { FlowStep } from "@/lib/types";

const flow: Record<string, FlowStep> = {
  fetch: { action: "curl" } as FlowStep,
  build: { action: "make", depends_on: ["fetch"] } as FlowStep,
};

describe("WorkflowDag fullscreen", () => {
  it("renders inline without a dialog", () => {
    render(<WorkflowDag flow={flow} selectedStep={null} onSelectStep={() => {}} />);

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("button", { name: /expand graph/i })).toBeInTheDocument();
  });

  it("opens a fullscreen dialog showing the same steps", () => {
    render(<WorkflowDag flow={flow} selectedStep={null} onSelectStep={() => {}} />);

    fireEvent.click(screen.getByRole("button", { name: /expand graph/i }));

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getAllByText("fetch").length).toBeGreaterThan(0);
    expect(within(dialog).getAllByText("build").length).toBeGreaterThan(0);
  });

  it("selecting a node in fullscreen propagates to the page", () => {
    const onSelectStep = vi.fn();
    render(<WorkflowDag flow={flow} selectedStep={null} onSelectStep={onSelectStep} />);

    fireEvent.click(screen.getByRole("button", { name: /expand graph/i }));
    const dialog = screen.getByRole("dialog");
    fireEvent.click(within(dialog).getAllByText("build")[0]);

    expect(onSelectStep).toHaveBeenCalledWith("build");
  });

  it("closes with the collapse button", () => {
    render(<WorkflowDag flow={flow} selectedStep={null} onSelectStep={() => {}} />);

    fireEvent.click(screen.getByRole("button", { name: /expand graph/i }));
    fireEvent.click(screen.getByRole("button", { name: /exit fullscreen/i }));

    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("renders without duplicate edge ids when a name is referenced from two branches", () => {
    // `{any: [{all: [A, B]}, {all: [A, C]}]}` legitimately references "a"
    // twice — collectDependsOnNames preserves duplicates, but the edge
    // builder must dedupe them into one `a->merge` edge.
    const flowWithDuplicateDep: Record<string, FlowStep> = {
      a: { action: "curl" } as FlowStep,
      b: { action: "curl" } as FlowStep,
      c: { action: "curl" } as FlowStep,
      merge: {
        action: "make",
        depends_on: [
          { any: [{ all: ["a", "b"] }, { all: ["a", "c"] }] },
        ],
      } as FlowStep,
    };
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <WorkflowDag flow={flowWithDuplicateDep} selectedStep={null} onSelectStep={() => {}} />,
    );
    expect(screen.getByText("merge")).toBeInTheDocument();
    expect(errorSpy).not.toHaveBeenCalled();
    errorSpy.mockRestore();
  });
});
