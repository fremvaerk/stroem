import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return { ...actual, apiFetchRaw: vi.fn() };
});

import { apiFetchRaw } from "@/lib/api";
import { InputFileList } from "./input-file-list";
import type { InputFileRef } from "@/lib/types";

const files: InputFileRef[] = [
  { field: "data_file", file_id: "file_a", name: "report.csv", size_bytes: 2048, content_type: "text/csv" },
  { field: "pic/1", file_id: "file_b", name: "p.png", size_bytes: 10, content_type: "image/png" },
];

describe("InputFileList", () => {
  beforeEach(() => {
    vi.mocked(apiFetchRaw).mockResolvedValue({ blob: async () => new Blob(["x"]) } as Response);
    URL.createObjectURL = vi.fn(() => "blob:x");
    URL.revokeObjectURL = vi.fn();
  });
  afterEach(() => vi.clearAllMocks());

  it("renders nothing when empty or undefined", () => {
    const { container, rerender } = render(<InputFileList jobId="j" items={[]} />);
    expect(container.firstChild).toBeNull();
    rerender(<InputFileList jobId="j" items={undefined} />);
    expect(container.firstChild).toBeNull();
  });

  it("renders a row per file with name, size, type and field", () => {
    render(<InputFileList jobId="j" items={files} />);
    expect(screen.getByRole("heading", { name: "Input files" })).toBeTruthy();
    expect(screen.getByText("report.csv")).toBeTruthy();
    expect(screen.getByText("2.0 KB")).toBeTruthy();
    expect(screen.getByText("text/csv")).toBeTruthy();
    expect(screen.getByText(/data_file/)).toBeTruthy();
  });

  it("downloads through apiFetchRaw with the encoded field", async () => {
    render(<InputFileList jobId="job-1" items={files} />);
    fireEvent.click(screen.getByRole("button", { name: "Download report.csv" }));
    await waitFor(() => expect(apiFetchRaw).toHaveBeenCalledWith("/api/jobs/job-1/input-files/data_file"));
    fireEvent.click(screen.getByRole("button", { name: "Open p.png in new tab" }));
    await waitFor(() => expect(apiFetchRaw).toHaveBeenCalledWith("/api/jobs/job-1/input-files/pic%2F1"));
  });

  it("shows an error when the download fails", async () => {
    vi.mocked(apiFetchRaw).mockRejectedValue(new Error("boom"));
    render(<InputFileList jobId="j" items={files} />);
    fireEvent.click(screen.getByRole("button", { name: "Download report.csv" }));
    expect((await screen.findByRole("alert")).textContent).toContain("boom");
  });
});
