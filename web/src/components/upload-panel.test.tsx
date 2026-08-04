import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import {
  STORAGE_KEY,
  UploadPausedError,
  UploadProtocolError,
  UploadRetryableError,
  saveStoredUpload,
  type UploadSession,
  type uploadFile,
} from "../upload-client";
import { UploadPanel } from "./upload-panel";

const active: UploadSession = {
  id: "upload-one", projectId: "project-one", fileName: "notes.bin",
  totalSize: "10", committedOffset: "4", state: "active",
};
const complete: UploadSession = { ...active, committedOffset: "10", state: "complete" };

function asRunner(mock: ReturnType<typeof vi.fn>) {
  return mock as unknown as typeof uploadFile;
}

describe("UploadPanel", () => {
  beforeEach(() => localStorage.clear());

  it("uploads a picked file, shows determinate progress, and refreshes only after completion", async () => {
    const onComplete = vi.fn();
    const runner = vi.fn(async (options) => {
      options.onProgress?.({ committed: 4, total: 10, session: active });
      await Promise.resolve();
      options.onProgress?.({ committed: 10, total: 10, session: complete });
      return complete;
    });
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={onComplete} upload={asRunner(runner)} />);

    const file = new File([new Uint8Array(10)], "notes.bin");
    await user.upload(screen.getByLabelText("Choose a file to upload"), file);

    expect(await screen.findByRole("progressbar", { name: "Upload progress" })).toHaveAttribute("aria-valuenow", "100");
    expect(screen.getByText("10 B of 10 B")).toBeVisible();
    await waitFor(() => expect(onComplete).toHaveBeenCalledOnce());
    expect(screen.getByRole("status")).toHaveTextContent("Upload complete");
  });

  it("accepts a dropped file and prevents a second upload while active", async () => {
    let finish!: (session: UploadSession) => void;
    const runner = vi.fn(() => new Promise<UploadSession>((resolve) => { finish = resolve; }));
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(runner)} />);
    const file = new File(["data"], "drop.txt");

    const dropTarget = screen.getByTestId("upload-drop-target");
    fireEvent.drop(dropTarget, { dataTransfer: { files: [file] } });

    await waitFor(() => expect(runner).toHaveBeenCalledOnce());
    const disabledInput = screen.getByLabelText("Choose a file to upload");
    expect(disabledInput).toBeDisabled();
    await userEvent.upload(disabledInput, new File(["other"], "other.txt"));
    expect(screen.queryByTestId("upload-drop-target")).not.toBeInTheDocument();
    expect(runner).toHaveBeenCalledOnce();
    finish({ ...complete, fileName: "drop.txt", totalSize: "4", committedOffset: "4" });
  });

  it("pauses on connectivity failure and retries the same file and session", async () => {
    const runner = vi.fn()
      .mockRejectedValueOnce(new UploadPausedError(active))
      .mockResolvedValueOnce(complete);
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(runner)} />);
    const file = new File([new Uint8Array(10)], "notes.bin");

    await user.upload(screen.getByLabelText("Choose a file to upload"), file);
    expect(await screen.findByRole("alert")).toHaveTextContent("connection was interrupted");
    await user.click(screen.getByRole("button", { name: "Retry upload" }));

    await waitFor(() => expect(runner).toHaveBeenCalledTimes(2));
    expect(runner.mock.calls[1][0]).toMatchObject({ file, session: active, projectId: "project-one" });
  });

  it("shows a reload recovery prompt and resumes only an exact reselection", async () => {
    saveStoredUpload({ uploadId: "upload-one", projectId: "project-one", fileName: "notes.bin", totalSize: "10", committedOffset: "4" });
    const runner = vi.fn().mockResolvedValue(complete);
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(runner)} />);

    expect(screen.getByRole("status")).toHaveTextContent("Paused upload found");
    expect(screen.getByRole("button", { name: "Choose same file" })).toBeVisible();
    await user.upload(screen.getByLabelText("Choose a file to upload"), new File(["wrong"], "wrong.bin"));

    expect(await screen.findByRole("alert")).toHaveTextContent("does not match");
    expect(localStorage.getItem(STORAGE_KEY)).not.toBeNull();
    expect(runner).not.toHaveBeenCalled();

    await user.upload(screen.getByLabelText("Choose a file to upload"), new File([new Uint8Array(10)], "notes.bin"));
    await waitFor(() => expect(runner).toHaveBeenCalledOnce());
    expect(runner.mock.calls[0][0].session).toMatchObject({ id: "upload-one", committedOffset: "4" });
  });

  it("retains a recovered session across project changes until explicit dismiss", async () => {
    saveStoredUpload({ uploadId: "upload-one", projectId: "project-one", fileName: "notes.bin", totalSize: "10", committedOffset: "4" });
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-two" projectName="Other" onComplete={vi.fn()} upload={asRunner(vi.fn())} />);

    expect(screen.getByRole("status")).toHaveTextContent("belongs to another project");
    await user.click(screen.getByRole("button", { name: "Dismiss recovered upload" }));
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(screen.queryByText(/Paused upload found/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Upload file" })).toHaveFocus();
  });

  it("opens the picker from a keyboard-accessible upload button", async () => {
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(vi.fn())} />);
    const input = screen.getByLabelText("Choose a file to upload");
    const click = vi.spyOn(input, "click");
    await user.click(screen.getByRole("button", { name: "Upload file" }));
    expect(click).toHaveBeenCalledOnce();
  });

  it("retries finalization with the same file and session until complete, then refreshes once", async () => {
    const finalizing = { ...active, committedOffset: "10", state: "finalizing" as const };
    const runner = vi.fn()
      .mockRejectedValueOnce(new UploadRetryableError(finalizing, "Finalization is still in progress."))
      .mockRejectedValueOnce(new UploadRetryableError(finalizing, "Finalization is still in progress."))
      .mockResolvedValueOnce(complete);
    const onComplete = vi.fn();
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={onComplete} upload={asRunner(runner)} />);
    const file = new File([new Uint8Array(10)], "notes.bin");

    await user.upload(screen.getByLabelText("Choose a file to upload"), file);
    expect(await screen.findByRole("alert")).toHaveTextContent("Finalization is still in progress");
    expect(onComplete).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Retry upload" }));
    expect(await screen.findByRole("button", { name: "Retry upload" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Retry upload" }));

    await waitFor(() => expect(onComplete).toHaveBeenCalledOnce());
    expect(runner).toHaveBeenCalledTimes(3);
    expect(runner.mock.calls.slice(1).every((call) => call[0].file === file)).toBe(true);
    expect(runner.mock.calls.slice(1).every((call) => call[0].session.id === "upload-one")).toBe(true);
  });

  it("explicitly discards a retryable upload and clears only its persisted recovery state", async () => {
    saveStoredUpload({ uploadId: "upload-one", projectId: "project-one", fileName: "notes.bin", totalSize: "10", committedOffset: "4" });
    const runner = vi.fn().mockRejectedValue(new UploadRetryableError(active, "Please retry."));
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(runner)} />);

    await user.upload(screen.getByLabelText("Choose a file to upload"), new File([new Uint8Array(10)], "notes.bin"));
    expect(await screen.findByRole("button", { name: "Discard upload" })).toBeVisible();
    expect(localStorage.getItem(STORAGE_KEY)).not.toBeNull();

    await user.click(screen.getByRole("button", { name: "Discard upload" }));
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(screen.getByRole("button", { name: "Upload file" })).toBeVisible();
    expect(screen.getByLabelText("Choose a file to upload")).toBeEnabled();
    expect(screen.getByRole("button", { name: "Upload file" })).toHaveFocus();
  });

  it("restores focus to Upload file after cancelling an active upload", async () => {
    const runner = vi.fn(() => new Promise<UploadSession>(() => undefined));
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(runner)} />);
    await user.upload(screen.getByLabelText("Choose a file to upload"), new File(["data"], "notes.bin"));

    await user.click(await screen.findByRole("button", { name: "Cancel upload" }));

    expect(screen.getByRole("button", { name: "Upload file" })).toHaveFocus();
  });

  it("offers Dismiss rather than Retry for a terminal upload failure", async () => {
    const runner = vi.fn().mockRejectedValue(new UploadProtocolError("This upload is no longer available."));
    const user = userEvent.setup();
    render(<UploadPanel projectId="project-one" projectName="Records" onComplete={vi.fn()} upload={asRunner(runner)} />);

    await user.upload(screen.getByLabelText("Choose a file to upload"), new File([new Uint8Array(10)], "notes.bin"));
    expect(await screen.findByRole("alert")).toHaveTextContent("This upload is no longer available");
    expect(screen.queryByRole("button", { name: "Retry upload" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Discard upload" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Dismiss" })).toBeVisible();
  });
});
