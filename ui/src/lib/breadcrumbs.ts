export interface Crumb {
  label: string;
  href: string;
  isLast: boolean;
}

const sectionLabels: Record<string, string> = {
  workspaces: "Workspaces",
  tasks: "Tasks",
  jobs: "Jobs",
  workers: "Workers",
  users: "Users",
  settings: "Settings",
  approve: "Approve",
};

/**
 * Derive breadcrumbs from a pathname. Every crumb links to a real route:
 * `/workspaces/:ws/tasks/:name` collapses to `Workspaces / ws / name`
 * because `/workspaces/:ws/tasks` is not a page of its own.
 */
export function buildBreadcrumbs(pathname: string): Crumb[] {
  const segments = pathname.split("/").filter(Boolean);
  if (segments.length === 0) {
    return [{ label: "Dashboard", href: "/", isLast: true }];
  }

  const crumbs: Crumb[] = [];
  let path = "";
  for (let i = 0; i < segments.length; i++) {
    const segment = segments[i];
    path += `/${segment}`;
    const isLast = i === segments.length - 1;
    const isWorkspaceTasksSegment =
      segments[0] === "workspaces" && i === 2 && segment === "tasks";
    if (isWorkspaceTasksSegment && !isLast) continue;
    const label =
      i === 0
        ? (sectionLabels[segment] ?? decodeURIComponent(segment))
        : decodeURIComponent(segment);
    crumbs.push({ label, href: path, isLast });
  }
  return crumbs;
}
