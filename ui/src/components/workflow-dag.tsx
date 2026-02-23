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
  SkipForward,
  XCircle,
} from "lucide-react";
import { cn } from "@/lib/utils";
import type { JobStep, FlowStep } from "@/lib/types";

const NODE_WIDTH = 200;
const NODE_HEIGHT = 72;

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
        data.status ? statusBorderColor[data.status] ?? "border-border" : "border-border",
        data.selected && "ring-2 ring-ring ring-offset-2 ring-offset-background",
      )}
      style={{ width: NODE_WIDTH, minHeight: NODE_HEIGHT - 16 }}
    >
      <Handle type="target" position={Position.Left} className="!bg-border !h-2 !w-2" />
      <div className="flex items-center gap-1.5">
        {data.status && (statusIcon[data.status] ?? <Circle className="h-3.5 w-3.5 text-muted-foreground" />)}
        <span className="truncate font-mono text-sm font-medium">{data.label}</span>
      </div>
      {data.action && (
        <p className="mt-0.5 truncate text-xs text-muted-foreground">{data.action}</p>
      )}
      {data.status && (
        <p className="mt-0.5 text-xs capitalize text-muted-foreground">{data.status}</p>
      )}
      <Handle type="source" position={Position.Right} className="!bg-border !h-2 !w-2" />
    </div>
  );
}

const nodeTypes = { step: StepNode };

function getLayoutedElements(
  nodes: Node<StepNodeData>[],
  edges: Edge[],
): { nodes: Node<StepNodeData>[]; edges: Edge[] } {
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 100 });

  for (const node of nodes) {
    g.setNode(node.id, { width: NODE_WIDTH, height: NODE_HEIGHT });
  }
  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }

  dagre.layout(g);

  const layoutedNodes = nodes.map((node) => {
    const pos = g.node(node.id);
    return {
      ...node,
      position: {
        x: pos.x - NODE_WIDTH / 2,
        y: pos.y - NODE_HEIGHT / 2,
      },
    };
  });

  return { nodes: layoutedNodes, edges };
}

function buildEdgeColor(status?: string): string {
  if (status === "completed") return "hsl(var(--chart-2))";
  if (status === "running") return "hsl(var(--chart-1))";
  return "hsl(var(--border))";
}

interface WorkflowDagProps {
  steps?: JobStep[];
  flow?: Record<string, FlowStep>;
  selectedStep: string | null;
  onSelectStep: (stepName: string | null) => void;
}

export function WorkflowDag({
  steps,
  flow,
  selectedStep,
  onSelectStep,
}: WorkflowDagProps) {
  const { nodes, edges } = useMemo(() => {
    const rawNodes: Node<StepNodeData>[] = [];
    const rawEdges: Edge[] = [];

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
          },
        });
        for (const dep of step.depends_on ?? []) {
          const sourceStatus = statusMap.get(dep);
          rawEdges.push({
            id: `${dep}->${step.step_name}`,
            source: dep,
            target: step.step_name,
            type: "smoothstep",
            animated: sourceStatus === "completed" && step.status === "running",
            style: { stroke: buildEdgeColor(sourceStatus) },
          });
        }
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
          },
        });
        for (const dep of flowStep.depends_on ?? []) {
          rawEdges.push({
            id: `${dep}->${name}`,
            source: dep,
            target: name,
            type: "smoothstep",
            style: { stroke: "hsl(var(--border))" },
          });
        }
      }
    }

    return getLayoutedElements(rawNodes, rawEdges);
  }, [steps, flow, selectedStep]);

  const onNodeClick = useCallback(
    (_: React.MouseEvent, node: Node) => {
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
