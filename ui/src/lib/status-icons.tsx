import { CheckCircle2, XCircle, Loader2, Clock, SkipForward, Circle } from "lucide-react";

/**
 * Status icon mapping for job steps. Used by StepTimeline and WorkflowDag.
 * Icons use h-4 w-4 sizing (standard list view).
 */
export const statusIcons: Record<string, React.ReactNode> = {
  pending: <Clock className="h-4 w-4 text-muted-foreground" />,
  ready: <Circle className="h-4 w-4 text-blue-500" />,
  running: <Loader2 className="h-4 w-4 animate-spin text-blue-500" />,
  completed: <CheckCircle2 className="h-4 w-4 text-green-500" />,
  failed: <XCircle className="h-4 w-4 text-red-500" />,
  skipped: <SkipForward className="h-4 w-4 text-muted-foreground" />,
};

/**
 * Status icon mapping for the DAG graph view (smaller icons: h-3.5 w-3.5).
 */
export const statusIconsSmall: Record<string, React.ReactNode> = {
  pending: <Clock className="h-3.5 w-3.5 text-muted-foreground" />,
  ready: <Circle className="h-3.5 w-3.5 text-blue-500" />,
  running: <Loader2 className="h-3.5 w-3.5 animate-spin text-blue-500" />,
  completed: <CheckCircle2 className="h-3.5 w-3.5 text-green-500" />,
  failed: <XCircle className="h-3.5 w-3.5 text-red-500" />,
  skipped: <SkipForward className="h-3.5 w-3.5 text-muted-foreground" />,
};
