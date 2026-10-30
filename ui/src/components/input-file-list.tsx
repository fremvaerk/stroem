import { useState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ApiError } from "@/lib/api";
import type { InputFileRef } from "@/lib/types";
import { SAFE_INLINE, humanSize, iconFor, triggerArtifactAction } from "@/components/artifact-helpers";

/** The job page's "Input files" card: files supplied to `file` input fields. */
export function InputFileList({ jobId, items }: { jobId: string; items: InputFileRef[] | undefined }) {
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});

  if (!items || items.length === 0) return null;

  async function handleClick(f: InputFileRef, inline: boolean) {
    const key = f.field;
    setBusy((b) => ({ ...b, [key]: true }));
    setErrors((e) => {
      const { [key]: _drop, ...rest } = e;
      void _drop;
      return rest;
    });
    try {
      await triggerArtifactAction(
        `/api/jobs/${encodeURIComponent(jobId)}/input-files/${encodeURIComponent(f.field)}`,
        f.name,
        inline,
      );
    } catch (e) {
      const msg =
        e instanceof ApiError ? `${e.status}: ${e.message}` : e instanceof Error ? e.message : "Download failed";
      setErrors((prev) => ({ ...prev, [key]: msg }));
    } finally {
      setBusy((b) => {
        const { [key]: _drop, ...rest } = b;
        void _drop;
        return rest;
      });
    }
  }

  return (
    <section aria-labelledby="input-files-heading" className="rounded-lg border bg-card p-4">
      <h2 id="input-files-heading" className="mb-3 font-semibold">
        Input files
      </h2>
      <ul className="divide-y">
        {items.map((f) => {
          const ct = f.content_type.split(";")[0].trim().toLowerCase();
          const safe = SAFE_INLINE.has(ct);
          const isBusy = !!busy[f.field];
          const error = errors[f.field];
          return (
            <li key={f.field} className="flex flex-wrap items-center gap-3 py-2">
              {iconFor(ct)}
              <span className="flex-1 truncate font-mono text-sm" title={f.name}>
                {f.name}
              </span>
              <span className="tabular-nums text-xs text-muted-foreground">{humanSize(f.size_bytes)}</span>
              <span className="hidden text-xs text-muted-foreground md:inline">{ct}</span>
              <span className="hidden text-xs text-muted-foreground md:inline">field: {f.field}</span>
              <Button
                variant="outline"
                size="sm"
                aria-label={safe ? `Open ${f.name} in new tab` : `Download ${f.name}`}
                disabled={isBusy}
                onClick={() => handleClick(f, safe)}
              >
                {isBusy ? (
                  <>
                    <Loader2 className="mr-1 h-3 w-3 animate-spin" aria-hidden />
                    {safe ? "Opening…" : "Downloading…"}
                  </>
                ) : safe ? (
                  "Open ↗"
                ) : (
                  "Download"
                )}
              </Button>
              {error && (
                <p role="alert" className="basis-full text-xs text-destructive">
                  {error}
                </p>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
