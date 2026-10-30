import type { InputFileRef } from "./types";

/** A `file` field's state in the run form (spec 2026-10-29-file-inputs § 7). */
export type FileFieldState =
  | { kind: "file"; mode: "empty" }
  | { kind: "file"; mode: "uploading"; name: string; size: number; progress: number }
  | { kind: "file"; mode: "failed"; name: string; error: string }
  | { kind: "file"; mode: "ready"; fileId: string; name: string; size: number; fromSource: boolean };

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
