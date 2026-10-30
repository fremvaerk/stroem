import type { InputFileRef } from "./types";

/** A `file` field's state in the run form (spec 2026-10-29-file-inputs § 7). */
export type FileFieldState =
  | { kind: "file"; mode: "empty" }
  | { kind: "file"; mode: "uploading"; name: string; size: number; progress: number }
  | { kind: "file"; mode: "failed"; name: string; error: string }
  | { kind: "file"; mode: "ready"; fileId: string; name: string; size: number; fromSource: boolean };

/** A field that holds an uploaded (or re-run source) file. */
export type ReadyFileFieldState = Extract<FileFieldState, { mode: "ready" }>;

export function isFileFieldState(v: unknown): v is FileFieldState {
  return typeof v === "object" && v !== null && (v as { kind?: unknown }).kind === "file";
}

/** `ready` + `fromSource` when the re-run source supplied a file, else `empty`. */
export function initialFileFieldState(source?: InputFileRef): FileFieldState {
  if (source) {
    return {
      kind: "file",
      mode: "ready",
      fileId: source.file_id,
      name: source.name,
      size: source.size_bytes,
      fromSource: true,
    };
  }
  return { kind: "file", mode: "empty" };
}

/** True while an upload runs or after it failed: Run must stay disabled. */
export function fileFieldBlocksRun(v: unknown): boolean {
  return isFileFieldState(v) && (v.mode === "uploading" || v.mode === "failed");
}

/**
 * The file an upload started from `current` replaces: the `ready` file it
 * starts from; after a failed upload, still the file the first attempt
 * replaced (`previous`); from `empty`, none.
 */
export function replacedByUpload(
  current: FileFieldState,
  previous: ReadyFileFieldState | null,
): ReadyFileFieldState | null {
  switch (current.mode) {
    case "ready":
      return current;
    case "empty":
      return null;
    default:
      return previous;
  }
}

/**
 * Where a Replace that did not finish (a cancelled upload, or Remove after a
 * failed one) ends: the file it replaced — in a Re-run, the source file —
 * else `empty`.
 */
export function afterAbandonedUpload(replaced: ReadyFileFieldState | null): FileFieldState {
  return replaced ?? { kind: "file", mode: "empty" };
}
