import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";

import { ProjectCreateDialog } from "./project-create-dialog";
import type { Project } from "../types";

const createdProject: Project = {
  id: "new-project",
  name: "New records",
  createdAt: "2026-08-04T10:00:00Z",
};

const originalShowModal = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const originalClose = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
const showModalMock = vi.fn(function (this: HTMLDialogElement) {
  if (this.open) throw new DOMException("The dialog is already open.", "InvalidStateError");
  this.setAttribute("open", "");
});
const closeMock = vi.fn(function (this: HTMLDialogElement) {
  this.removeAttribute("open");
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((promiseResolve, promiseReject) => {
    resolve = promiseResolve;
    reject = promiseReject;
  });
  return { promise, resolve, reject };
}

function DialogHarness({
  onCreate = vi.fn().mockResolvedValue(createdProject),
  onClose = vi.fn(),
}: {
  onCreate?: (name: string) => Promise<Project>;
  onClose?: () => void;
}) {
  return <ProjectCreateDialog open onCreate={onCreate} onClose={onClose} />;
}

function StatefulDialogHarness() {
  const [open, setOpen] = useState(true);
  return <ProjectCreateDialog open={open} onCreate={vi.fn().mockResolvedValue(createdProject)} onClose={() => setOpen(false)} />;
}

describe("ProjectCreateDialog", () => {
  beforeAll(() => {
    Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: showModalMock });
    Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: closeMock });
  });

  beforeEach(() => {
    showModalMock.mockClear();
    closeMock.mockClear();
  });

  afterAll(() => {
    if (originalShowModal) Object.defineProperty(HTMLDialogElement.prototype, "showModal", originalShowModal);
    else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
    if (originalClose) Object.defineProperty(HTMLDialogElement.prototype, "close", originalClose);
    else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  });

  it("opens with the native modal API once across repeated open renders", () => {
    const { rerender } = render(<DialogHarness />);

    expect(showModalMock).toHaveBeenCalledOnce();
    expect(screen.getByRole("dialog")).toHaveProperty("open", true);

    rerender(<DialogHarness />);
    expect(showModalMock).toHaveBeenCalledOnce();
  });

  it("closes the native modal when the parent closes it", () => {
    const { rerender } = render(<DialogHarness />);

    rerender(<ProjectCreateDialog open={false} onCreate={vi.fn()} onClose={vi.fn()} />);

    expect(closeMock).toHaveBeenCalledOnce();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("handles the native cancel event through parent state", () => {
    render(<StatefulDialogHarness />);

    fireEvent(screen.getByRole("dialog"), new Event("cancel", { bubbles: true, cancelable: true }));

    expect(closeMock).toHaveBeenCalledOnce();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("focuses the labeled name input and closes with Escape", async () => {
    const onClose = vi.fn();
    const user = userEvent.setup();
    render(<DialogHarness onClose={onClose} />);

    const input = screen.getByRole("textbox", { name: "Project name" });
    expect(input).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("cancels without submitting and restores focus to the opener", async () => {
    const onCreate = vi.fn().mockResolvedValue(createdProject);
    const user = userEvent.setup();
    const opener = document.createElement("button");
    opener.textContent = "Open";
    document.body.append(opener);
    opener.focus();
    const { rerender } = render(<DialogHarness onCreate={onCreate} />);

    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onCreate).not.toHaveBeenCalled();

    rerender(<ProjectCreateDialog open={false} onCreate={onCreate} onClose={vi.fn()} />);
    expect(opener).toHaveFocus();
    opener.remove();
  });

  it("validates the trimmed 1 to 100 Unicode code-point boundary", async () => {
    const onCreate = vi.fn().mockResolvedValue(createdProject);
    const user = userEvent.setup();
    render(<DialogHarness onCreate={onCreate} />);

    await user.click(screen.getByRole("button", { name: "Create project" }));
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a project name.");

    const input = screen.getByRole("textbox", { name: "Project name" });
    await user.type(input, " 保存庫 ");
    await user.click(screen.getByRole("button", { name: "Create project" }));
    expect(onCreate).toHaveBeenLastCalledWith("保存庫");
  });

  it("accepts 100 and rejects 101 Unicode code points", async () => {
    const onCreate = vi.fn().mockResolvedValue(createdProject);
    const user = userEvent.setup();
    const { unmount } = render(<DialogHarness onCreate={onCreate} />);
    const input = screen.getByRole("textbox", { name: "Project name" });

    fireEvent.change(input, { target: { value: "🗂️".repeat(50) } });
    await user.click(screen.getByRole("button", { name: "Create project" }));
    expect(onCreate).toHaveBeenCalledOnce();

    unmount();
    render(<DialogHarness onCreate={onCreate} />);
    const longInput = screen.getByRole("textbox", { name: "Project name" });
    fireEvent.change(longInput, { target: { value: "📁".repeat(101) } });
    await user.click(screen.getByRole("button", { name: "Create project" }));
    expect(onCreate).toHaveBeenCalledOnce();
    expect(screen.getByRole("alert")).toHaveTextContent("100 characters or fewer");
  });

  it("disables controls while pending and prevents duplicate submission", async () => {
    const pending = deferred<Project>();
    const onCreate = vi.fn(() => pending.promise);
    const user = userEvent.setup();
    render(<DialogHarness onCreate={onCreate} />);

    await user.type(screen.getByRole("textbox", { name: "Project name" }), "Records");
    const submit = screen.getByRole("button", { name: "Create project" });
    await user.dblClick(submit);

    expect(onCreate).toHaveBeenCalledOnce();
    expect(screen.getByRole("textbox", { name: "Project name" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Creating project" })).toBeDisabled();
  });

  it("announces a safe server error and remains open", async () => {
    const onCreate = vi.fn().mockRejectedValue(new Error("Project creation is temporarily unavailable."));
    const user = userEvent.setup();
    render(<DialogHarness onCreate={onCreate} />);

    await user.type(screen.getByRole("textbox", { name: "Project name" }), "Records");
    await user.click(screen.getByRole("button", { name: "Create project" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("Project creation is temporarily unavailable.");
    expect(screen.getByRole("dialog")).toBeVisible();
  });

  it("resets after a successful submission before reopening", async () => {
    const onCreate = vi.fn().mockResolvedValue(createdProject);
    const user = userEvent.setup();
    const { rerender } = render(<ProjectCreateDialog open onCreate={onCreate} onClose={vi.fn()} />);

    await user.type(screen.getByRole("textbox", { name: "Project name" }), "Records");
    await user.click(screen.getByRole("button", { name: "Create project" }));
    rerender(<ProjectCreateDialog open={false} onCreate={onCreate} onClose={vi.fn()} />);
    rerender(<ProjectCreateDialog open onCreate={onCreate} onClose={vi.fn()} />);

    expect(screen.getByRole("textbox", { name: "Project name" })).toBeEnabled();
    expect(screen.getByRole("textbox", { name: "Project name" })).toHaveValue("");
  });
});
