interface JsonViewerProps {
  data: Record<string, unknown> | null;
}

export function JsonViewer({ data }: JsonViewerProps) {
  if (!data) {
    return (
      <div className="rounded-lg bg-zinc-950 p-4">
        <span className="font-mono text-xs text-zinc-600">No data</span>
      </div>
    );
  }

  return (
    <pre className="max-h-[400px] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed text-zinc-300">
      {JSON.stringify(data, null, 2)}
    </pre>
  );
}
