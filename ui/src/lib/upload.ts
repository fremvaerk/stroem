import { ApiError, getAccessToken, tryRestoreSession } from "@/lib/api";

export interface UploadedFile {
  file_id: string;
  name: string;
  size_bytes: number;
  sha256: string;
  content_type: string;
  expires_at: string;
}

export interface UploadOptions {
  /** The Re-run source job: its task is the one checked for a pinned source. */
  sourceJobId?: string;
  onProgress?: (fraction: number) => void;
  signal?: AbortSignal;
}

interface XhrResult {
  status: number;
  statusText: string;
  text: string;
}

function abortError(): Error {
  const e = new Error("Upload cancelled");
  e.name = "AbortError";
  return e;
}

function send(url: string, file: File, token: string | null, opts: UploadOptions): Promise<XhrResult> {
  return new Promise((resolve, reject) => {
    if (opts.signal?.aborted) {
      reject(abortError());
      return;
    }
    const xhr = new XMLHttpRequest();
    xhr.open("POST", url);
    if (token) xhr.setRequestHeader("Authorization", `Bearer ${token}`);
    xhr.setRequestHeader("Content-Type", file.type || "application/octet-stream");
    const onAbort = () => xhr.abort();
    opts.signal?.addEventListener("abort", onAbort);
    const done = () => opts.signal?.removeEventListener("abort", onAbort);
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable && e.total > 0) opts.onProgress?.(e.loaded / e.total);
    };
    xhr.onload = () => {
      done();
      resolve({ status: xhr.status, statusText: xhr.statusText, text: xhr.responseText });
    };
    xhr.onerror = () => {
      done();
      reject(new Error("Network error during upload"));
    };
    xhr.onabort = () => {
      done();
      reject(abortError());
    };
    xhr.send(file);
  });
}

/**
 * Upload one input file for a `file` field (spec 2026-10-29-file-inputs § 4.2).
 * `fetch` has no upload progress, so this uses `XMLHttpRequest`. `task` is the
 * task's `id`, like `executeTask`.
 */
export async function uploadInputFile(
  ws: string,
  task: string,
  file: File,
  opts: UploadOptions = {},
): Promise<UploadedFile> {
  let url = `/api/workspaces/${encodeURIComponent(ws)}/tasks/${encodeURIComponent(task)}/files?name=${encodeURIComponent(file.name)}`;
  if (opts.sourceJobId) url += `&source_job_id=${encodeURIComponent(opts.sourceJobId)}`;

  let token = getAccessToken();
  if (!token) {
    await tryRestoreSession();
    token = getAccessToken();
  }
  let res = await send(url, file, token, opts);
  if (res.status === 401) {
    await tryRestoreSession();
    res = await send(url, file, getAccessToken(), opts);
  }
  if (res.status >= 200 && res.status < 300) {
    return JSON.parse(res.text) as UploadedFile;
  }
  let message = res.statusText;
  try {
    const body = JSON.parse(res.text) as { error?: string };
    if (body.error) message = body.error;
  } catch {
    // keep statusText
  }
  throw new ApiError(res.status, message);
}
