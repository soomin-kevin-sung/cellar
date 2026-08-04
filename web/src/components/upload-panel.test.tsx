import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { UploadPanel } from "./upload-panel";
import type { uploadFile } from "../upload-client";

function asUploader(mock: ReturnType<typeof vi.fn>) {
  return mock as unknown as typeof uploadFile;
}

describe("UploadPanel", () => {
  it("starts with one simple file picker", () => {
    render(<UploadPanel onComplete={vi.fn()} projectId="project-one" projectName="Records" />);

    expect(screen.getByText("Drop one file here")).toBeVisible();
    expect(screen.getByRole("button", { name: "Upload file" })).toBeVisible();
    expect(screen.queryByText(/paused|resume|recover/i)).not.toBeInTheDocument();
  });

  it("uploads once and refreshes the completed file list", async () => {
    let finish!: (value: { name: string; size: string }) => void;
    const uploader = vi.fn(() => new Promise<{ name: string; size: string }>((resolve) => { finish = resolve; }));
    const onComplete = vi.fn();
    const user = userEvent.setup();
    render(<UploadPanel onComplete={onComplete} projectId="project-one" projectName="Records" upload={asUploader(uploader)} />);
    const file = new File(["cellar"], "notes.txt");

    await user.upload(screen.getByLabelText("Choose a file to upload"), file);
    expect(screen.getByRole("status")).toHaveTextContent("Uploading");
    expect(uploader).toHaveBeenCalledWith(expect.objectContaining({ projectId: "project-one", file }));

    await act(async () => finish({ name: file.name, size: String(file.size) }));
    expect(await screen.findByRole("status")).toHaveTextContent("Upload complete");
    expect(onComplete).toHaveBeenCalledOnce();
  });

  it("retries a failed upload from the beginning", async () => {
    const uploader = vi.fn()
      .mockRejectedValueOnce(new Error("The connection was interrupted."))
      .mockResolvedValueOnce({ name: "notes.txt", size: "6" });
    const user = userEvent.setup();
    render(<UploadPanel onComplete={vi.fn()} projectId="project-one" projectName="Records" upload={asUploader(uploader)} />);
    const file = new File(["cellar"], "notes.txt");

    await user.upload(screen.getByLabelText("Choose a file to upload"), file);
    expect(await screen.findByRole("alert")).toHaveTextContent("The connection was interrupted.");
    await user.click(screen.getByRole("button", { name: "Retry upload" }));

    expect(await screen.findByRole("status")).toHaveTextContent("Upload complete");
    expect(uploader).toHaveBeenCalledTimes(2);
    expect(uploader.mock.calls[1][0].file).toBe(file);
  });

  it("accepts a dropped file", () => {
    const uploader = vi.fn().mockResolvedValue({ name: "drop.txt", size: "4" });
    render(<UploadPanel onComplete={vi.fn()} projectId="project-one" projectName="Records" upload={asUploader(uploader)} />);
    const file = new File(["drop"], "drop.txt");

    fireEvent.drop(screen.getByTestId("upload-drop-target"), { dataTransfer: { files: [file] } });

    expect(uploader).toHaveBeenCalledWith(expect.objectContaining({ file }));
  });
});
