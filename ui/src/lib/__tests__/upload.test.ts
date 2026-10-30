import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return { ...actual, getAccessToken: vi.fn(), tryRestoreSession: vi.fn() };
});

import { ApiError, getAccessToken, tryRestoreSession } from "@/lib/api";
import { uploadInputFile } from "../upload";

class FakeXhr {
  static instances: FakeXhr[] = [];
  method = "";
  url = "";
  headers: Record<string, string> = {};
  body: unknown = null;
  status = 0;
  statusText = "";
  responseText = "";
  aborted = false;
  upload: { onprogress: ((e: { lengthComputable: boolean; loaded: number; total: number }) => void) | null } = {
    onprogress: null,
  };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  constructor() {
    FakeXhr.instances.push(this);
  }
  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }
  setRequestHeader(k: string, v: string) {
    this.headers[k] = v;
  }
  send(body: unknown) {
    this.body = body;
  }
  abort() {
    this.aborted = true;
    this.onabort?.();
  }
  respond(status: number, body: unknown, statusText = "") {
    this.status = status;
    this.statusText = statusText;
    this.responseText = typeof body === "string" ? body : JSON.stringify(body);
    this.onload?.();
  }
}

const OK = {
  file_id: "file_" + "a".repeat(32),
  name: "r.csv",
  size_bytes: 3,
  sha256: "x",
  content_type: "text/csv",
  expires_at: "2026-11-01T00:00:00Z",
};
const tick = () => new Promise((r) => setTimeout(r, 0));

describe("uploadInputFile", () => {
  beforeEach(() => {
    FakeXhr.instances = [];
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    vi.mocked(getAccessToken).mockReturnValue("tok");
    vi.mocked(tryRestoreSession).mockResolvedValue(true);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("posts the raw file with Bearer and content type, encodes the URL", async () => {
    const file = new File(["abc"], "my report.csv", { type: "text/csv" });
    const p = uploadInputFile("ws 1", "task/x", file);
    await tick();
    const x = FakeXhr.instances[0];
    expect(x.method).toBe("POST");
    expect(x.url).toBe("/api/workspaces/ws%201/tasks/task%2Fx/files?name=my%20report.csv");
    expect(x.headers["Authorization"]).toBe("Bearer tok");
    expect(x.headers["Content-Type"]).toBe("text/csv");
    expect(x.body).toBe(file);
    x.respond(201, OK);
    await expect(p).resolves.toEqual(OK);
  });

  it("falls back to application/octet-stream", async () => {
    const p = uploadInputFile("w", "t", new File(["a"], "a.bin"));
    await tick();
    expect(FakeXhr.instances[0].headers["Content-Type"]).toBe("application/octet-stream");
    FakeXhr.instances[0].respond(201, OK);
    await p;
  });

  it("puts source_job_id in the URL", async () => {
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"), { sourceJobId: "job-1" });
    await tick();
    expect(FakeXhr.instances[0].url).toBe("/api/workspaces/w/tasks/t/files?name=a.txt&source_job_id=job-1");
    FakeXhr.instances[0].respond(201, OK);
    await p;
  });

  it("reports progress as a fraction", async () => {
    const onProgress = vi.fn();
    const p = uploadInputFile("w", "t", new File(["abcd"], "a.txt"), { onProgress });
    await tick();
    const x = FakeXhr.instances[0];
    x.upload.onprogress?.({ lengthComputable: true, loaded: 1, total: 4 });
    x.upload.onprogress?.({ lengthComputable: false, loaded: 2, total: 0 });
    expect(onProgress).toHaveBeenCalledTimes(1);
    expect(onProgress).toHaveBeenCalledWith(0.25);
    x.respond(201, OK);
    await p;
  });

  it("restores the session when there is no token", async () => {
    vi.mocked(getAccessToken).mockReturnValueOnce(null).mockReturnValue("fresh");
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"));
    await tick();
    expect(tryRestoreSession).toHaveBeenCalledTimes(1);
    expect(FakeXhr.instances[0].headers["Authorization"]).toBe("Bearer fresh");
    FakeXhr.instances[0].respond(201, OK);
    await p;
  });

  it("refreshes and retries once on 401", async () => {
    vi.mocked(getAccessToken).mockReturnValueOnce("old").mockReturnValue("new");
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"));
    await tick();
    FakeXhr.instances[0].respond(401, { error: "expired" });
    await tick();
    expect(tryRestoreSession).toHaveBeenCalledTimes(1);
    expect(FakeXhr.instances).toHaveLength(2);
    expect(FakeXhr.instances[1].headers["Authorization"]).toBe("Bearer new");
    FakeXhr.instances[1].respond(201, OK);
    await expect(p).resolves.toEqual(OK);
  });

  it("does not retry a second 401", async () => {
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"));
    p.catch(() => {});
    await tick();
    FakeXhr.instances[0].respond(401, { error: "no" });
    await tick();
    FakeXhr.instances[1].respond(401, { error: "still no" });
    await expect(p).rejects.toMatchObject({ status: 401, message: "still no" });
    expect(FakeXhr.instances).toHaveLength(2);
  });

  it("maps 413 to an ApiError with the server text", async () => {
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"));
    await tick();
    FakeXhr.instances[0].respond(413, { error: "file is larger than 10 bytes" });
    const err = await p.catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(413);
    expect(err.message).toBe("file is larger than 10 bytes");
  });

  it("falls back to statusText when the body is not JSON", async () => {
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"));
    await tick();
    FakeXhr.instances[0].respond(502, "<html>", "Bad Gateway");
    await expect(p).rejects.toMatchObject({ status: 502, message: "Bad Gateway" });
  });

  it("rejects on a network error", async () => {
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"));
    await tick();
    FakeXhr.instances[0].onerror?.();
    await expect(p).rejects.toThrow(/network/i);
  });

  it("aborts via the signal", async () => {
    const ac = new AbortController();
    const p = uploadInputFile("w", "t", new File(["a"], "a.txt"), { signal: ac.signal });
    p.catch(() => {});
    await tick();
    ac.abort();
    expect(FakeXhr.instances[0].aborted).toBe(true);
    await expect(p).rejects.toMatchObject({ name: "AbortError" });
  });
});
