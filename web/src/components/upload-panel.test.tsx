import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { UploadPanel, type UploadTask } from "./upload-panel";

const baseProps = {
  onReset: vi.fn(),
  onRetry: vi.fn(),
  onUpload: vi.fn(),
  projectId: "project-one",
  projectName: "Records",
  task: null,
};

function task(change: Partial<UploadTask> = {}): UploadTask {
  return {
    projectId: "project-one",
    projectName: "Records",
    file: new File(["cellar"], "notes.txt"),
    state: "uploading",
    progress: 42,
    result: null,
    message: "",
    ...change,
  };
}

describe("UploadPanel", () => {
  beforeEach(() => vi.clearAllMocks());

  it("starts with one simple file picker", () => {
    render(<UploadPanel {...baseProps} />);

    expect(screen.getByText("파일 하나를 여기에 놓으세요")).toBeVisible();
    expect(screen.getByRole("button", { name: "파일 선택" })).toBeVisible();
  });

  it("passes the selected file to the persistent upload owner", async () => {
    const onUpload = vi.fn();
    const user = userEvent.setup();
    render(<UploadPanel {...baseProps} onUpload={onUpload} />);
    const file = new File(["cellar"], "notes.txt");

    await user.upload(screen.getByLabelText("업로드할 파일 선택"), file);
    expect(onUpload).toHaveBeenCalledWith(file);
  });

  it("renders committed background progress supplied by the app", () => {
    render(<UploadPanel {...baseProps} task={task()} />);

    expect(screen.getByRole("status")).toHaveTextContent("업로드 중 42%");
    expect(screen.getByRole("progressbar")).toHaveValue(42);
    expect(screen.getByText("다른 화면에서도 계속 전송됩니다")).toBeVisible();
  });

  it("retries a failed upload from the beginning", async () => {
    const onRetry = vi.fn();
    const user = userEvent.setup();
    render(<UploadPanel {...baseProps} onRetry={onRetry} task={task({ state: "error", message: "연결이 끊겼습니다." })} />);

    expect(screen.getByRole("alert")).toHaveTextContent("연결이 끊겼습니다.");
    await user.click(screen.getByRole("button", { name: "처음부터 다시 업로드" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it("accepts a dropped file", () => {
    const onUpload = vi.fn();
    render(<UploadPanel {...baseProps} onUpload={onUpload} />);
    const file = new File(["drop"], "drop.txt");

    fireEvent.drop(screen.getByTestId("upload-drop-target"), { dataTransfer: { files: [file] } });
    expect(onUpload).toHaveBeenCalledWith(file);
  });
});
