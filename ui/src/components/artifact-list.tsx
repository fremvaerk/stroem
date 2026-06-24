import { useState } from "react";
import {
  FileText,
  Image as ImageIcon,
  FileArchive,
  FileCode,
  File,
  Loader2,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { apiFetchRaw, ApiError, type ArtifactItem } from "@/lib/api";

// Content types that the server marks as inline (Content-Disposition: inline).
// Mirrors the SAFE_INLINE list in crates/stroem-server/src/web/api/artifacts.rs.
// For these we open in a new tab; everything else triggers a download.
const SAFE_INLINE = new Set([
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
  "application/pdf",
  "text/plain",
  "text/markdown",
]);

// Exact-match archive content types — using a Set avoids loose `.endsWith`
// matches like `application/notatar` being treated as a tarball.
const ARCHIVE_TYPES = new Set([
  "application/zip",
  "application/gzip",
  "application/x-gzip",
  "application/x-tar",
  "application/x-bzip2",
  "application/x-7z-compressed",
  "application/x-rar-compressed",
]);

// Exact-match code/data content types (keep `text/*` prefix as the catch-all).
const CODE_TYPES = new Set([
  "application/json",
  "application/yaml",
  "application/x-yaml",
  "application/xml",
]);

function iconFor(ct: string) {
  if (ct.startsWith("image/")) return <ImageIcon className="h-4 w-4" aria-hidden />;
  if (ct === "application/pdf") return <FileText className="h-4 w-4" aria-hidden />;
  if (ARCHIVE_TYPES.has(ct))
    return <FileArchive className="h-4 w-4" aria-hidden />;
  if (ct.startsWith("text/") || CODE_TYPES.has(ct))
    return <FileCode className="h-4 w-4" aria-hidden />;
  return <File className="h-4 w-4" aria-hidden />;
}

function humanSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

/**
 * Fetch an artifact with the SPA's JWT attached, build a `blob:` URL,
 * and either open it in a new tab (safe MIME) or trigger a download.
 *
 * Direct `<a href={artifact.url}>` navigation can't do this because the
 * JWT lives in-memory (not in a cookie) — the browser would fire an
 * unauthenticated GET and the server's `require_auth` middleware would
 * return 401, surfacing as a JSON error page instead of the file.
 */
async function triggerArtifactAction(
  url: string,
  filename: string,
  inline: boolean,
): Promise<void> {
  const res = await apiFetchRaw(url);
  const blob = await res.blob();
  const objectUrl = URL.createObjectURL(blob);
  try {
    if (inline) {
      // `noopener` blocks the new tab from reaching back into this window.
      // The blob URL is same-origin to the SPA so the browser can render
      // PDFs / images inline without re-fetching (and without needing auth
      // a second time).
      window.open(objectUrl, "_blank", "noopener");
    } else {
      const a = document.createElement("a");
      a.href = objectUrl;
      a.download = filename;
      // Some browsers require the anchor to be in the DOM before .click()
      // — append, click, remove in the same tick.
      document.body.appendChild(a);
      a.click();
      a.remove();
    }
  } finally {
    // Give the browser a generous window to actually start the download /
    // render the inline view before we revoke. 60s is plenty for both
    // cases without leaking the blob indefinitely.
    setTimeout(() => URL.revokeObjectURL(objectUrl), 60_000);
  }
}

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
