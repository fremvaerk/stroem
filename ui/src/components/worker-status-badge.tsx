import { Badge } from "@/components/ui/badge";

const workerStatusStyles: Record<string, string> = {
  active: "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400",
  inactive: "bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400",
  stale: "bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400",
};

export function WorkerStatusBadge({ status }: { status: string }) {
  return (
    <Badge
      variant="secondary"
      className={workerStatusStyles[status] ?? "bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400"}
    >
      {status === "active" && (
        <span className="mr-1.5 inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-current" />
      )}
      {status}
    </Badge>
  );
}
