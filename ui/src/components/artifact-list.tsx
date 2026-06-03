import {
  FileText,
  Image as ImageIcon,
  FileArchive,
  FileCode,
  File,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import type { ArtifactItem } from "@/lib/api";

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

export function ArtifactList({
  items,
  fetchError = false,
}: {
  items: ArtifactItem[];
  fetchError?: boolean;
}) {
  // Render nothing when there are no artifacts AND no fetch error — keeps the
  // empty-job common case visually quiet. When fetchError is true, show the
  // section header with a muted error row so a 403/500 is distinguishable
  // from "no artifacts produced".
  if (items.length === 0 && !fetchError) return null;
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
            const label = safe
              ? `Open ${a.name} in new tab`
              : `Download ${a.name}`;
            return (
              <li key={a.name} className="flex items-center gap-3 py-2">
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
                <Button asChild variant="outline" size="sm">
                  <a
                    href={a.url}
                    target={safe ? "_blank" : undefined}
                    rel="noopener noreferrer"
                    aria-label={label}
                    {...(safe ? {} : { download: a.name })}
                  >
                    {safe ? "Open ↗" : "Download"}
                  </a>
                </Button>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
