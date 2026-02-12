import { useCallback, useEffect, useRef, useState } from "react";
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "@/components/ui/tabs";
import { LogViewer } from "@/components/log-viewer";
import { JsonViewer } from "@/components/json-viewer";
import { getStepLogs } from "@/lib/api";
import type { JobStep } from "@/lib/types";

interface StepDetailProps {
  jobId: string;
  step: JobStep;
}

export function StepDetail({ jobId, step }: StepDetailProps) {
  const [logs, setLogs] = useState("");
  const [loadingLogs, setLoadingLogs] = useState(true);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchLogs = useCallback(async () => {
    try {
      const data = await getStepLogs(jobId, step.step_name);
      setLogs(data.logs);
    } catch {
      // Silently handle — logs may not exist yet
    } finally {
      setLoadingLogs(false);
    }
  }, [jobId, step.step_name]);

  useEffect(() => {
    fetchLogs();

    // Poll while step is running
    const isActive = step.status === "running" || step.status === "ready";
    if (isActive) {
      intervalRef.current = setInterval(fetchLogs, 2000);
    }

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
      }
    };
  }, [fetchLogs, step.status]);

  const isStreaming = step.status === "running";

  return (
    <Tabs defaultValue="logs" className="w-full">
      <TabsList>
        <TabsTrigger value="logs">Logs</TabsTrigger>
        <TabsTrigger value="input">Input</TabsTrigger>
        <TabsTrigger value="output">Output</TabsTrigger>
      </TabsList>
      <TabsContent value="logs">
        {loadingLogs ? (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-muted border-t-primary" />
          </div>
        ) : (
          <LogViewer logs={logs} isStreaming={isStreaming} />
        )}
      </TabsContent>
      <TabsContent value="input">
        <JsonViewer data={step.input} />
      </TabsContent>
      <TabsContent value="output">
        <JsonViewer data={step.output} />
      </TabsContent>
    </Tabs>
  );
}
