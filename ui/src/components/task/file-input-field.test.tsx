import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";

vi.mock("@/lib/upload", () => ({ uploadInputFile: vi.fn() }));

import { uploadInputFile } from "@/lib/upload";
import { FileInputField } from "./file-input-field";
import { InputFieldRow } from "./input-field-row";
import { fileFieldBlocksRun, initialFileFieldState, type FileFieldState } from "@/lib/file-field";

const mockUpload = vi.mocked(uploadInputFile);

function Stateful({
  initial = initialFileFieldState(),
  sourceJobId,
  spy,
}: {
  initial?: FileFieldState;
  sourceJobId?: string;
  spy?: (v: FileFieldState) => void;
}) {
  const [v, setV] = useState<FileFieldState>(initial);
  return (
    <FileInputField
      id="input-data_file"
      fieldKey="data_file"
      field={{ type: "file", required: true, description: "The data" }}
      value={v}
      onChange={(n) => {
        spy?.(n);
        setV(n);
      }}
      workspace="default"
      taskId="file-demo"
      sourceJobId={sourceJobId}
    />
  );
}

const picker = () => document.getElementById("input-data_file") as HTMLInputElement;
const pick = (file: File) => fireEvent.change(picker(), { target: { files: [file] } });

describe("FileInputField", () => {
  beforeEach(() => {
    mockUpload.mockReset();
  });

  it("uploads on pick, shows progress, then name and size", async () => {
    let onProgress: ((f: number) => void) | undefined;
    let resolve!: (v: Awaited<ReturnType<typeof uploadInputFile>>) => void;
    mockUpload.mockImplementation((_ws, _task, _file, opts) => {
      onProgress = opts?.onProgress;
      return new Promise((r) => {
        resolve = r;
      });
    });
    render(<Stateful />);
    pick(new File(["x".repeat(2048)], "report.csv", { type: "text/csv" }));
    expect(mockUpload).toHaveBeenCalledWith("default", "file-demo", expect.any(File), expect.objectContaining({}));
    expect(screen.getByText("report.csv")).toBeTruthy();
    expect(screen.getByRole("progressbar")).toBeTruthy();
    act(() => onProgress?.(0.5));
    expect(screen.getByRole("progressbar").getAttribute("aria-valuenow")).toBe("50");
    await act(async () => {
      resolve({
        file_id: "file_" + "b".repeat(32),
        name: "report.csv",
        size_bytes: 2048,
        sha256: "s",
        content_type: "text/csv",
        expires_at: "2026-11-01T00:00:00Z",
      });
    });
    expect(screen.queryByRole("progressbar")).toBeNull();
    expect(screen.getByText("report.csv")).toBeTruthy();
    expect(screen.getByText(/2\.0 KB/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Remove" })).toBeTruthy();
  });

  it("emits a ready state carrying the file id", async () => {
    const spy = vi.fn();
    mockUpload.mockResolvedValue({
      file_id: "file_" + "c".repeat(32),
      name: "a.txt",
      size_bytes: 1,
      sha256: "s",
      content_type: "text/plain",
      expires_at: "x",
    });
    render(<Stateful spy={spy} />);
    pick(new File(["a"], "a.txt"));
    await waitFor(() =>
      expect(spy).toHaveBeenLastCalledWith({
        kind: "file",
        mode: "ready",
        fileId: "file_" + "c".repeat(32),
        name: "a.txt",
        size: 1,
        fromSource: false,
      }),
    );
  });

  it("Remove returns to empty", async () => {
    render(
      <Stateful initial={{ kind: "file", mode: "ready", fileId: "file_x", name: "a.txt", size: 1, fromSource: false }} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(screen.queryByText("a.txt")).toBeNull();
    expect(screen.getByRole("button", { name: "Choose file" })).toBeTruthy();
  });

  it("shows the server error when the upload fails", async () => {
    mockUpload.mockRejectedValue(new Error("file is larger than 10 bytes"));
    render(<Stateful />);
    pick(new File(["abc"], "big.bin"));
    expect((await screen.findByRole("alert")).textContent).toContain("file is larger than 10 bytes");
    expect(screen.getByRole("button", { name: "Choose file" })).toBeTruthy();
  });

  it("re-run source shows the previous file and Replace uploads with the source job id", async () => {
    mockUpload.mockResolvedValue({
      file_id: "file_" + "d".repeat(32),
      name: "new.csv",
      size_bytes: 3,
      sha256: "s",
      content_type: "text/csv",
      expires_at: "x",
    });
    render(
      <Stateful
        sourceJobId="job-9"
        initial={initialFileFieldState({
          field: "data_file",
          file_id: "file_old",
          name: "old.csv",
          size_bytes: 10,
          content_type: "text/csv",
        })}
      />,
    );
    expect(screen.getByText("old.csv")).toBeTruthy();
    expect(screen.getByText("From the previous run")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Replace" })).toBeTruthy();
    pick(new File(["new"], "new.csv"));
    expect(mockUpload).toHaveBeenCalledWith(
      "default",
      "file-demo",
      expect.any(File),
      expect.objectContaining({ sourceJobId: "job-9" }),
    );
    await waitFor(() => expect(screen.getByText("new.csv")).toBeTruthy());
    expect(screen.queryByText("From the previous run")).toBeNull();
  });

  it("a failed upload offers Remove, which empties the field and unblocks Run", async () => {
    const spy = vi.fn();
    mockUpload.mockRejectedValue(new Error("file is larger than 10 bytes"));
    render(<Stateful spy={spy} />);
    pick(new File(["abc"], "big.bin"));
    await screen.findByRole("alert");
    expect(fileFieldBlocksRun(spy.mock.lastCall?.[0])).toBe(true);
    expect(screen.getByRole("button", { name: "Choose file" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(spy).toHaveBeenLastCalledWith({ kind: "file", mode: "empty" });
    expect(fileFieldBlocksRun(spy.mock.lastCall?.[0])).toBe(false);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByText("big.bin")).toBeNull();
  });

  const SOURCE = initialFileFieldState({
    field: "data_file",
    file_id: "file_old",
    name: "old.csv",
    size_bytes: 10,
    content_type: "text/csv",
  });

  it("Cancel of a Replace upload restores the source file", async () => {
    const spy = vi.fn();
    mockUpload.mockImplementation(() => new Promise(() => {}));
    render(<Stateful sourceJobId="job-9" initial={SOURCE} spy={spy} />);
    pick(new File(["new"], "new.csv"));
    expect(screen.getByRole("progressbar")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(spy).toHaveBeenLastCalledWith(SOURCE);
    expect(screen.getByText("old.csv")).toBeTruthy();
    expect(screen.getByText("From the previous run")).toBeTruthy();
    expect(fileFieldBlocksRun(spy.mock.lastCall?.[0])).toBe(false);
  });

  it("Remove after a failed Replace restores the source file, also after a second failure", async () => {
    const spy = vi.fn();
    mockUpload.mockRejectedValue(new Error("file is larger than 10 bytes"));
    render(<Stateful sourceJobId="job-9" initial={SOURCE} spy={spy} />);
    pick(new File(["new"], "new.csv"));
    await screen.findByRole("alert");
    // A second attempt from the failed state still replaces the source.
    pick(new File(["newer"], "newer.csv"));
    await waitFor(() => expect(screen.getByText("newer.csv")).toBeTruthy());
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(spy).toHaveBeenLastCalledWith(SOURCE);
    expect(screen.getByText("old.csv")).toBeTruthy();
    expect(screen.getByText("From the previous run")).toBeTruthy();
  });

  it("after Remove of the source file, a cancelled upload ends empty", async () => {
    const spy = vi.fn();
    mockUpload.mockImplementation(() => new Promise(() => {}));
    render(<Stateful sourceJobId="job-9" initial={SOURCE} spy={spy} />);
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    pick(new File(["new"], "new.csv"));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(spy).toHaveBeenLastCalledWith({ kind: "file", mode: "empty" });
    expect(screen.queryByText("old.csv")).toBeNull();
    expect(screen.getByRole("button", { name: "Choose file" })).toBeTruthy();
  });

  it("InputFieldRow routes type file to the file widget, ignoring secret and options", () => {
    render(
      <InputFieldRow
        fieldKey="data_file"
        field={{ type: "file", secret: true, options: ["a", "b"], multiple: true }}
        value={initialFileFieldState()}
        onChange={() => {}}
        workspace="default"
        taskId="t"
      />,
    );
    expect(screen.getByRole("button", { name: "Choose file" })).toBeTruthy();
    expect(document.querySelector("input[type=password]")).toBeNull();
  });
});
