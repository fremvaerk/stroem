import {
  FileText,
  Image as ImageIcon,
  FileArchive,
  FileCode,
  File,
} from "lucide-react";
import { apiFetchRaw } from "@/lib/api";

// Content types that the server marks as inline (Content-Disposition: inline).
// Mirrors the SAFE_INLINE list in crates/stroem-server/src/web/api/artifacts.rs.
// For these we open in a new tab; everything else triggers a download.
export const SAFE_INLINE = new Set([
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

export function iconFor(ct: string) {
  if (ct.startsWith("image/")) return <ImageIcon className="h-4 w-4" aria-hidden />;
  if (ct === "application/pdf") return <FileText className="h-4 w-4" aria-hidden />;
  if (ARCHIVE_TYPES.has(ct))
    return <FileArchive className="h-4 w-4" aria-hidden />;
  if (ct.startsWith("text/") || CODE_TYPES.has(ct))
    return <FileCode className="h-4 w-4" aria-hidden />;
  return <File className="h-4 w-4" aria-hidden />;
}

export function humanSize(bytes: number) {
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
export async function triggerArtifactAction(
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

