import { describe, it, expect } from "vitest";
import { buildBreadcrumbs } from "../breadcrumbs";

describe("buildBreadcrumbs", () => {
  it("returns Dashboard for the root", () => {
    expect(buildBreadcrumbs("/")).toEqual([
      { label: "Dashboard", href: "/", isLast: true },
    ]);
  });

  it("labels top-level sections", () => {
    expect(buildBreadcrumbs("/workspaces")).toEqual([
      { label: "Workspaces", href: "/workspaces", isLast: true },
    ]);
    expect(buildBreadcrumbs("/workers/abc").map((c) => c.label)).toEqual([
      "Workers",
      "abc",
    ]);
  });

  it("links the workspace crumb to the workspace page", () => {
    expect(buildBreadcrumbs("/workspaces/dwelltime_isolated")).toEqual([
      { label: "Workspaces", href: "/workspaces", isLast: false },
      {
        label: "dwelltime_isolated",
        href: "/workspaces/dwelltime_isolated",
        isLast: true,
      },
    ]);
  });

  it("drops the dead 'tasks' segment on a task detail path and decodes names", () => {
    expect(buildBreadcrumbs("/workspaces/prod/tasks/say%20hello")).toEqual([
      { label: "Workspaces", href: "/workspaces", isLast: false },
      { label: "prod", href: "/workspaces/prod", isLast: false },
      {
        label: "say hello",
        href: "/workspaces/prod/tasks/say%20hello",
        isLast: true,
      },
    ]);
  });

  it("does not touch a 'tasks' segment elsewhere", () => {
    expect(buildBreadcrumbs("/tasks").map((c) => c.label)).toEqual(["Tasks"]);
    expect(buildBreadcrumbs("/jobs/tasks").map((c) => c.label)).toEqual([
      "Jobs",
      "tasks",
    ]);
  });
});
