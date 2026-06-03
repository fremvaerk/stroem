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

function iconFor(ct: string) {
  if (ct.startsWith("image/")) return <ImageIcon className="h-4 w-4" aria-hidden />;
  if (ct === "application/pdf") return <FileText className="h-4 w-4" aria-hidden />;
  if (ct === "application/zip" || ct.endsWith("gzip") || ct.endsWith("tar"))
    return <FileArchive className="h-4 w-4" aria-hidden />;
  if (ct.startsWith("text/") || ct.endsWith("json") || ct.endsWith("yaml"))
    return <FileCode className="h-4 w-4" aria-hidden />;
  return <File className="h-4 w-4" aria-hidden />;
}

function humanSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

export function ArtifactList({ items }: { items: ArtifactItem[] }) {
  if (items.length === 0) return null;
  return (
    <section
      aria-labelledby="artifacts-heading"
      className="rounded-lg border bg-card p-4"
    >
      <h2 id="artifacts-heading" className="mb-3 font-semibold">
        Artifacts
      </h2>
      <ul className="divide-y">
        {items.map((a) => {
          const ct = a.content_type.split(";")[0].trim().toLowerCase();
          const safe = SAFE_INLINE.has(ct);
          return (
            <li key={a.name} className="flex items-center gap-3 py-2">
              {iconFor(ct)}
              <span className="flex-1 truncate font-mono text-sm">{a.name}</span>
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
                  rel="noreferrer"
                >
                  {safe ? "Open ↗" : "Download"}
                </a>
              </Button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
