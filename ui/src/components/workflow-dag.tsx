import { useCallback, useMemo } from "react";
import {
  ReactFlow,
  Controls,
  type Node,
  type Edge,
  type NodeProps,
  Handle,
  Position,
} from "@xyflow/react";
import dagre from "@dagrejs/dagre";
import {
  CheckCircle2,
  Circle,
  Clock,
  Loader2,
  Play,
  Flag,
  SkipForward,
  XCircle,
} from "lucide-react";
import { cn } from "@/lib/utils";
import type { JobStep, FlowStep } from "@/lib/types";

const NODE_WIDTH = 200;
const NODE_HEIGHT = 72;
const SENTINEL_WIDTH = 80;
const SENTINEL_HEIGHT = NODE_HEIGHT;

// --- Edge colors: use raw CSS variable values (oklch), no hsl() wrapper ---
const EDGE_COLOR_DEFAULT = "oklch(0.7 0 0)";
const EDGE_COLOR_COMPLETED = "oklch(0.6 0.15 160)";
const EDGE_COLOR_RUNNING = "oklch(0.6 0.15 260)";

const statusIcon: Record<string, React.ReactNode> = {
  pending: <Clock className="h-3.5 w-3.5 text-muted-foreground" />,
  ready: <Circle className="h-3.5 w-3.5 text-blue-500" />,
  running: <Loader2 className="h-3.5 w-3.5 animate-spin text-blue-500" />,
  completed: <CheckCircle2 className="h-3.5 w-3.5 text-green-500" />,
  failed: <XCircle className="h-3.5 w-3.5 text-red-500" />,
  skipped: <SkipForward className="h-3.5 w-3.5 text-muted-foreground" />,
};

const statusBorderColor: Record<string, string> = {
  pending: "border-yellow-400 dark:border-yellow-600",
  ready: "border-blue-400 dark:border-blue-500",
  running: "border-blue-400 dark:border-blue-500",
  completed: "border-green-400 dark:border-green-500",
  failed: "border-red-400 dark:border-red-500",
  skipped: "border-border",
  cancelled: "border-border",
};

interface StepNodeData {
  label: string;
  action?: string;
  status?: string;
  selected: boolean;
  [key: string]: unknown;
}

function StepNode({ data }: NodeProps<Node<StepNodeData>>) {
  return (
    <div
      className={cn(
        "rounded-lg border-2 bg-card px-3 py-2 shadow-sm transition-colors",
        data.status
          ? (statusBorderColor[data.status] ?? "border-border")
          : "border-border",
        data.selected && "ring-2 ring-ring ring-offset-2 ring-offset-background",
      )}
      style={{ width: NODE_WIDTH, minHeight: NODE_HEIGHT - 16 }}
    >
      <Handle
        type="target"
        position={Position.Left}
        className="!bg-border !h-2 !w-2"
      />
      <div className="flex items-center gap-1.5">
        {data.status &&
          (statusIcon[data.status] ?? (
            <Circle className="h-3.5 w-3.5 text-muted-foreground" />
          ))}
        <span className="truncate font-mono text-sm font-medium">
          {data.label}
        </span>
      </div>
      {data.action && (
        <p className="mt-0.5 truncate text-xs text-muted-foreground">
          {data.action}
        </p>
      )}
      {data.status && (
        <p className="mt-0.5 text-xs capitalize text-muted-foreground">
          {data.status}
        </p>
      )}
      <Handle
        type="source"
        position={Position.Right}
        className="!bg-border !h-2 !w-2"
      />
    </div>
  );
}

interface SentinelNodeData {
  label: string;
  variant: "start" | "finish";
  [key: string]: unknown;
}

function SentinelNode({ data }: NodeProps<Node<SentinelNodeData>>) {
  const isStart = data.variant === "start";
  return (
    <div
      className={cn(
        "flex items-center justify-center gap-1.5 rounded-full border bg-muted px-3 py-1.5 text-xs font-medium text-muted-foreground",
      )}
      style={{ width: SENTINEL_WIDTH, height: SENTINEL_HEIGHT }}
    >
      {isStart ? (
        <Play className="h-3 w-3" />
      ) : (
        <Flag className="h-3 w-3" />
      )}
      {data.label}
      {isStart && (
        <Handle
          type="source"
          position={Position.Right}
          className="!bg-border !h-2 !w-2"
        />
      )}
      {!isStart && (
        <Handle
          type="target"
          position={Position.Left}
          className="!bg-border !h-2 !w-2"
        />
      )}
    </div>
  );
}

const nodeTypes = { step: StepNode, sentinel: SentinelNode };

function getLayoutedElements(
  nodes: Node[],
  edges: Edge[],
): { nodes: Node[]; edges: Edge[] } {
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 100 });

  for (const node of nodes) {
    const isSentinel = node.type === "sentinel";
    g.setNode(node.id, {
      width: isSentinel ? SENTINEL_WIDTH : NODE_WIDTH,
      height: isSentinel ? SENTINEL_HEIGHT : NODE_HEIGHT,
    });
  }
  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }

  dagre.layout(g);

  const layoutedNodes = nodes.map((node) => {
    const pos = g.node(node.id);
    const isSentinel = node.type === "sentinel";
    const w = isSentinel ? SENTINEL_WIDTH : NODE_WIDTH;
    const h = isSentinel ? SENTINEL_HEIGHT : NODE_HEIGHT;
    return {
      ...node,
      position: { x: pos.x - w / 2, y: pos.y - h / 2 },
    };
  });

  return { nodes: layoutedNodes, edges };
}

function edgeColor(status?: string): string {
  if (status === "completed") return EDGE_COLOR_COMPLETED;
  if (status === "running") return EDGE_COLOR_RUNNING;
  return EDGE_COLOR_DEFAULT;
}

interface WorkflowDagProps {
  steps?: JobStep[];
  flow?: Record<string, FlowStep>;
  selectedStep: string | null;
  onSelectStep: (stepName: string | null) => void;
}

const START_ID = "__start__";
const FINISH_ID = "__finish__";

export function WorkflowDag({
  steps,
  flow,
  selectedStep,
  onSelectStep,
}: WorkflowDagProps) {
  const { nodes, edges } = useMemo(() => {
    const rawNodes: Node[] = [];
    const rawEdges: Edge[] = [];
    const targets = new Set<string>(); // nodes that are depended on
    const hasDeps = new Set<string>(); // nodes that have dependencies

    if (steps) {
      // Job mode: build from steps with status
      const statusMap = new Map(steps.map((s) => [s.step_name, s.status]));
      for (const step of steps) {
        rawNodes.push({
          id: step.step_name,
          type: "step",
          position: { x: 0, y: 0 },
          data: {
            label: step.step_name,
            action: step.action_name,
            status: step.status,
            selected: selectedStep === step.step_name,
          } satisfies StepNodeData,
        });
        for (const dep of step.depends_on ?? []) {
          targets.add(dep);
          hasDeps.add(step.step_name);
          const sourceStatus = statusMap.get(dep);
          rawEdges.push({
            id: `${dep}->${step.step_name}`,
            source: dep,
            target: step.step_name,
            type: "smoothstep",
            animated:
              sourceStatus === "completed" && step.status === "running",
            style: {
              stroke: edgeColor(sourceStatus),
              strokeWidth: 2,
            },
          });
        }
      }

      // Identify root nodes (no deps) and leaf nodes (not depended on)
      const allIds = new Set(steps.map((s) => s.step_name));
      const roots = [...allIds].filter((id) => !hasDeps.has(id));
      const leaves = [...allIds].filter((id) => !targets.has(id));

      // Add Start sentinel -> root nodes
      rawNodes.push({
        id: START_ID,
        type: "sentinel",
        position: { x: 0, y: 0 },
        data: { label: "Start", variant: "start" } satisfies SentinelNodeData,
      });
      for (const root of roots) {
        rawEdges.push({
          id: `${START_ID}->${root}`,
          source: START_ID,
          target: root,
          type: "smoothstep",
          style: { stroke: EDGE_COLOR_DEFAULT, strokeWidth: 2 },
        });
      }

      // Add leaf nodes -> Finish sentinel
      rawNodes.push({
        id: FINISH_ID,
        type: "sentinel",
        position: { x: 0, y: 0 },
        data: {
          label: "End",
          variant: "finish",
        } satisfies SentinelNodeData,
      });
      for (const leaf of leaves) {
        rawEdges.push({
          id: `${leaf}->${FINISH_ID}`,
          source: leaf,
          target: FINISH_ID,
          type: "smoothstep",
          style: {
            stroke: edgeColor(statusMap.get(leaf)),
            strokeWidth: 2,
          },
        });
      }
    } else if (flow) {
      // Task mode: build from flow definition (no status)
      for (const [name, flowStep] of Object.entries(flow)) {
        rawNodes.push({
          id: name,
          type: "step",
          position: { x: 0, y: 0 },
          data: {
            label: name,
            action: flowStep.action,
            selected: selectedStep === name,
          } satisfies StepNodeData,
        });
        for (const dep of flowStep.depends_on ?? []) {
          targets.add(dep);
          hasDeps.add(name);
          rawEdges.push({
            id: `${dep}->${name}`,
            source: dep,
            target: name,
            type: "smoothstep",
            style: { stroke: EDGE_COLOR_DEFAULT, strokeWidth: 2 },
          });
        }
      }

      const allIds = new Set(Object.keys(flow));
      const roots = [...allIds].filter((id) => !hasDeps.has(id));
      const leaves = [...allIds].filter((id) => !targets.has(id));

      rawNodes.push({
        id: START_ID,
        type: "sentinel",
        position: { x: 0, y: 0 },
        data: { label: "Start", variant: "start" } satisfies SentinelNodeData,
      });
      for (const root of roots) {
        rawEdges.push({
          id: `${START_ID}->${root}`,
          source: START_ID,
          target: root,
          type: "smoothstep",
          style: { stroke: EDGE_COLOR_DEFAULT, strokeWidth: 2 },
        });
      }

      rawNodes.push({
        id: FINISH_ID,
        type: "sentinel",
        position: { x: 0, y: 0 },
        data: {
          label: "End",
          variant: "finish",
        } satisfies SentinelNodeData,
      });
      for (const leaf of leaves) {
        rawEdges.push({
          id: `${leaf}->${FINISH_ID}`,
          source: leaf,
          target: FINISH_ID,
          type: "smoothstep",
          style: { stroke: EDGE_COLOR_DEFAULT, strokeWidth: 2 },
        });
      }
    }

    return getLayoutedElements(rawNodes, rawEdges);
  }, [steps, flow, selectedStep]);

  const onNodeClick = useCallback(
    (_: React.MouseEvent, node: Node) => {
      // Ignore clicks on sentinel nodes
      if (node.id === START_ID || node.id === FINISH_ID) return;
      onSelectStep(selectedStep === node.id ? null : node.id);
    },
    [selectedStep, onSelectStep],
  );

  return (
    <div className="h-80 w-full rounded-lg border bg-background">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        onNodeClick={onNodeClick}
        fitView
        fitViewOptions={{ padding: 0.2 }}
        nodesDraggable={false}
        nodesConnectable={false}
        elementsSelectable={false}
        proOptions={{ hideAttribution: true }}
        minZoom={0.3}
        maxZoom={2}
      >
        <Controls showInteractive={false} />
      </ReactFlow>
    </div>
  );
}
