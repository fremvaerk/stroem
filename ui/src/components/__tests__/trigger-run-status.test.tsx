import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import {
  LastRunCell,
  MissedRunsWarning,
} from "../trigger-run-status";
import type { TriggerInfo } from "@/lib/types";

function trig(over: Partial<TriggerInfo> = {}): TriggerInfo {
  return {
    name: "daily",
    type: "scheduler",
    cron: "0 6 * * *",
    task: "cleanup",
    enabled: true,
    input: {},
    next_runs: [],
    ...over,
  };
}

const wrap = (ui: React.ReactElement) =>
  render(<MemoryRouter>{ui}</MemoryRouter>);

describe("LastRunCell", () => {
  it("renders status and job link", () => {
    wrap(
      <LastRunCell
        trigger={trig({
          last_run: { job_id: "j1", created_at: "2026-10-20T06:00:00Z", status: "failed" },
        })}
      />,
    );
    expect(screen.getByText("failed")).toBeInTheDocument();
    expect(screen.getByRole("link")).toHaveAttribute("href", "/jobs/j1");
  });
  it("renders never", () => {
    wrap(<LastRunCell trigger={trig({ last_run: null })} />);
    expect(screen.getByText("never")).toBeInTheDocument();
  });
  it("renders never when fields are absent", () => {
    wrap(<LastRunCell trigger={trig()} />);
    expect(screen.getByText("never")).toBeInTheDocument();
  });
  it("renders no access", () => {
    wrap(<LastRunCell trigger={trig({ last_run: null, last_run_hidden: true })} />);
    expect(screen.getByText("no access")).toBeInTheDocument();
  });
  it("renders running since for an event source", () => {
    wrap(
      <LastRunCell
        trigger={trig({
          type: "event_source",
          last_run: { job_id: "j2", created_at: "2026-10-20T06:00:00Z", status: "running" },
        })}
      />,
    );
    expect(screen.getByText(/running since/)).toBeInTheDocument();
    expect(screen.getByRole("link")).toHaveAttribute("href", "/jobs/j2");
  });
});

describe("MissedRunsWarning", () => {
  const m = { count: 2, capped: false, first: "2026-10-20T06:00:00Z" };
  it("lists names", () => {
    wrap(<MissedRunsWarning triggers={[trig({ name: "a", missed: m }), trig({ name: "b", missed: m }), trig({ name: "c" })]} />);
    expect(screen.getByText(/2 triggers missed scheduled runs — a, b\./)).toBeInTheDocument();
  });
  it("uses singular", () => {
    wrap(<MissedRunsWarning triggers={[trig({ name: "a", missed: m })]} />);
    expect(screen.getByText(/1 trigger missed scheduled runs — a\./)).toBeInTheDocument();
  });
  it("renders nothing without missed", () => {
    const { container } = wrap(<MissedRunsWarning triggers={[trig()]} />);
    expect(container).toBeEmptyDOMElement();
  });
});
