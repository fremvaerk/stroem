import { useState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ApiError, type ArtifactItem } from "@/lib/api";
import { SAFE_INLINE, humanSize, iconFor, triggerArtifactAction } from "@/components/artifact-helpers";

export function ArtifactList({
  items,
  fetchError = false,
}: {
  items: ArtifactItem[];
  fetchError?: boolean;
}) {
  // Per-row UI state for download/open in flight + last error message. Keyed
  // by artifact name (unique per job by the DB constraint).
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});

  // Render nothing when there are no artifacts AND no fetch error — keeps the
  // empty-job common case visually quiet. When fetchError is true, show the
  // section header with a muted error row so a 403/500 is distinguishable
  // from "no artifacts produced".
  if (items.length === 0 && !fetchError) return null;

  async function handleClick(name: string, url: string, inline: boolean) {
    setBusy((b) => ({ ...b, [name]: true }));
    setErrors((e) => {
      const { [name]: _drop, ...rest } = e;
      void _drop;
      return rest;
    });
    try {
      await triggerArtifactAction(url, name, inline);
    } catch (e) {
      const msg =
        e instanceof ApiError
          ? `${e.status}: ${e.message}`
          : e instanceof Error
            ? e.message
            : "Download failed";
      setErrors((prev) => ({ ...prev, [name]: msg }));
    } finally {
      setBusy((b) => {
        const { [name]: _drop, ...rest } = b;
        void _drop;
        return rest;
      });
    }
  }

  return (
    <section
      aria-labelledby="artifacts-heading"
      className="rounded-lg border bg-card p-4"
    >
      <h2 id="artifacts-heading" className="mb-3 font-semibold">
        Artifacts
      </h2>
      {fetchError && items.length === 0 ? (
        <p
          role="status"
          className="text-sm text-muted-foreground"
          data-testid="artifacts-fetch-error"
        >
          Failed to load artifacts.
        </p>
      ) : (
        <ul className="divide-y">
          {items.map((a) => {
            const ct = a.content_type.split(";")[0].trim().toLowerCase();
            const safe = SAFE_INLINE.has(ct);
            const isBusy = !!busy[a.name];
            const error = errors[a.name];
            const label = safe
              ? `Open ${a.name} in new tab`
              : `Download ${a.name}`;
            return (
              <li
                key={a.name}
                className="flex flex-wrap items-center gap-3 py-2"
              >
                {iconFor(ct)}
                <span
                  className="flex-1 truncate font-mono text-sm"
                  title={a.name}
                >
                  {a.name}
                </span>
                <span className="tabular-nums text-xs text-muted-foreground">
                  {humanSize(a.size_bytes)}
                </span>
                <span className="hidden text-xs text-muted-foreground md:inline">
                  {ct}
                </span>
                <span className="hidden text-xs text-muted-foreground md:inline">
                  from: {a.step_name}
                </span>
                <Button
                  variant="outline"
                  size="sm"
                  aria-label={label}
                  disabled={isBusy}
                  onClick={() => handleClick(a.name, a.url, safe)}
                >
                  {isBusy ? (
                    <>
                      <Loader2
                        className="mr-1 h-3 w-3 animate-spin"
                        aria-hidden
                      />
                      {safe ? "Opening…" : "Downloading…"}
                    </>
                  ) : safe ? (
                    "Open ↗"
                  ) : (
                    "Download"
                  )}
                </Button>
                {error && (
                  <p
                    role="alert"
                    data-testid={`artifact-error-${a.name}`}
                    className="basis-full text-xs text-destructive"
                  >
                    {error}
                  </p>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
