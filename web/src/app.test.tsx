import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import App, { type ApiClient } from "./app";
import type { FileEntry, Project } from "./types";
import type { uploadFile } from "./upload-client";

const firstProject: Project = {
  id: "project-one",
  name: "Records",
  createdAt: "2026-08-04T10:00:00Z",
};
const secondProject: Project = {
  id: "project-two",
  name: "Field notes",
  createdAt: "2026-08-04T11:00:00Z",
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((promiseResolve, promiseReject) => {
    resolve = promiseResolve;
    reject = promiseReject;
  });
  return { promise, resolve, reject };
}

function makeClient(overrides: Partial<ApiClient> = {}): ApiClient {
  return {
    listProjects: vi.fn().mockResolvedValue([]),
    createProject: vi.fn().mockResolvedValue(firstProject),
    listFiles: vi.fn().mockResolvedValue([]),
    ...overrides,
  };
}

function asUploader(mock: ReturnType<typeof vi.fn>) {
  return mock as unknown as typeof uploadFile;
}

describe("App", () => {
  it("shows a status skeleton while projects load", () => {
    const pending = deferred<Project[]>();
    render(<App client={makeClient({ listProjects: vi.fn(() => pending.promise) })} />);

    expect(screen.getByRole("status", { name: "Loading projects" })).toBeVisible();
  });

  it("shows the brandless empty-first shell with one create action", async () => {
    render(<App client={makeClient()} />);

    expect(await screen.findByRole("heading", { name: "Create your first project" })).toBeVisible();
    expect(screen.getByText("Projects")).toBeVisible();
    expect(screen.getByText("Uploads")).toBeVisible();
    expect(screen.getByText("Settings")).toBeVisible();
    expect(screen.getAllByRole("button", { name: "Create project" })).toHaveLength(1);
    expect(document.body).not.toHaveTextContent(/Cellar|Photos/i);
    expect(document.querySelector('[aria-label*="logo" i]')).not.toBeInTheDocument();
  });

  it("shows a safe retryable project error", async () => {
    const listProjects = vi
      .fn()
      .mockRejectedValueOnce(new Error("Projects are temporarily unavailable."))
      .mockResolvedValueOnce([]);
    const user = userEvent.setup();
    render(<App client={makeClient({ listProjects })} />);

    expect(await screen.findByRole("alert")).toHaveTextContent("Projects are temporarily unavailable.");
    await user.click(screen.getByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("heading", { name: "Create your first project" })).toBeVisible();
    expect(listProjects).toHaveBeenCalledTimes(2);
  });

  it("renders only returned project names and exposes a mobile selector", async () => {
    render(<App client={makeClient({ listProjects: vi.fn().mockResolvedValue([firstProject, secondProject]) })} />);

    const navigation = await screen.findByRole("navigation", { name: "Projects navigation" });
    expect(within(navigation).getByRole("button", { name: "Records" })).toBeVisible();
    expect(within(navigation).getByRole("button", { name: "Field notes" })).toBeVisible();
    expect(within(navigation).queryByText("Photos")).not.toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Select project" })).toHaveValue("project-one");
  });

  it("loads files for project selection changes", async () => {
    const listFiles = vi.fn().mockResolvedValue([]);
    const user = userEvent.setup();
    render(
      <App
        client={makeClient({
          listProjects: vi.fn().mockResolvedValue([firstProject, secondProject]),
          listFiles,
        })}
      />,
    );

    await screen.findByRole("heading", { name: "Records" });
    await waitFor(() => expect(listFiles).toHaveBeenCalledWith("project-one", expect.any(AbortSignal)));
    await user.click(screen.getByRole("button", { name: "Field notes" }));

    expect(await screen.findByRole("heading", { name: "Field notes" })).toBeVisible();
    await waitFor(() => expect(listFiles).toHaveBeenCalledWith("project-two", expect.any(AbortSignal)));
    expect(screen.getByText("No files in this project yet.")).toBeVisible();
  });

  it("retries the current project file list and recovers to the empty state", async () => {
    const recovery = deferred<FileEntry[]>();
    const listFiles = vi
      .fn()
      .mockRejectedValueOnce(new Error("Files are temporarily unavailable."))
      .mockImplementationOnce(() => recovery.promise);
    const user = userEvent.setup();
    render(
      <App
        client={makeClient({
          listProjects: vi.fn().mockResolvedValue([firstProject]),
          listFiles,
        })}
      />,
    );

    expect(await screen.findByRole("alert")).toHaveTextContent("Files are temporarily unavailable.");
    expect(listFiles).toHaveBeenNthCalledWith(1, "project-one", expect.any(AbortSignal));

    await user.click(screen.getByRole("button", { name: "Retry loading files" }));
    expect(screen.getByRole("status")).toHaveTextContent("Loading files");
    expect(listFiles).toHaveBeenNthCalledWith(2, "project-one", expect.any(AbortSignal));

    await act(async () => recovery.resolve([]));
    expect(await screen.findByText("No files in this project yet.")).toBeVisible();
  });

  it("does not let a stale file response override the selected project", async () => {
    const firstFiles = deferred<FileEntry[]>();
    const secondFiles = deferred<FileEntry[]>();
    const listFiles = vi
      .fn()
      .mockImplementationOnce(() => firstFiles.promise)
      .mockImplementationOnce(() => secondFiles.promise);
    const user = userEvent.setup();
    render(
      <App
        client={makeClient({
          listProjects: vi.fn().mockResolvedValue([firstProject, secondProject]),
          listFiles,
        })}
      />,
    );

    await screen.findByRole("heading", { name: "Records" });
    await user.click(screen.getByRole("button", { name: "Field notes" }));
    await act(async () => {
      secondFiles.resolve([{ name: "current.txt", size: "3", modifiedAt: "2026-08-04T12:00:00Z" }]);
    });
    expect(await screen.findByText("current.txt")).toBeVisible();

    await act(async () => {
      firstFiles.resolve([{ name: "stale.txt", size: "5", modifiedAt: "2026-08-04T09:00:00Z" }]);
    });
    expect(screen.queryByText("stale.txt")).not.toBeInTheDocument();
    expect(screen.getByText("current.txt")).toBeVisible();
  });

  it("adds and selects a created project without reloading the project list", async () => {
    const created = { ...firstProject, id: "created-project", name: "New records" };
    const listProjects = vi.fn().mockResolvedValue([]);
    const listFiles = vi.fn().mockResolvedValue([]);
    const user = userEvent.setup();
    render(<App client={makeClient({ listProjects, listFiles, createProject: vi.fn().mockResolvedValue(created) })} />);

    await user.click(await screen.findByRole("button", { name: "Create project" }));
    await user.type(screen.getByRole("textbox", { name: "Project name" }), "  New records  ");
    await user.click(screen.getByRole("button", { name: "Create project" }));

    expect(await screen.findByRole("heading", { name: "New records" })).toBeVisible();
    await waitFor(() => expect(listFiles).toHaveBeenCalledWith("created-project", expect.any(AbortSignal)));
    expect(listProjects).toHaveBeenCalledOnce();
  });

  it("focuses the new workspace when first-project creation removes its opener", async () => {
    const created = { ...firstProject, id: "created-project", name: "New records" };
    const user = userEvent.setup();
    render(
      <App
        client={makeClient({
          createProject: vi.fn().mockResolvedValue(created),
          listFiles: vi.fn().mockResolvedValue([]),
        })}
      />,
    );

    await user.click(await screen.findByRole("button", { name: "Create project" }));
    await user.type(screen.getByRole("textbox", { name: "Project name" }), "New records");
    await user.click(screen.getByRole("button", { name: "Create project" }));

    const workspace = await screen.findByRole("region", { name: "New records" });
    expect(workspace).toHaveFocus();
  });

  it("restores focus to the connected create opener after cancelling", async () => {
    const user = userEvent.setup();
    render(
      <App
        client={makeClient({
          listProjects: vi.fn().mockResolvedValue([firstProject]),
        })}
      />,
    );

    const opener = await screen.findByRole("button", { name: "Create project" });
    await user.click(opener);
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(opener).toHaveFocus();
  });

  it("renders file metadata in a semantic linked table", async () => {
    render(<App client={makeClient({
      listProjects: vi.fn().mockResolvedValue([firstProject]),
      listFiles: vi.fn().mockResolvedValue([{ name: "notes.txt", size: "1536", modifiedAt: "2026-08-04T10:00:00Z" }]),
    })} />);

    const table = await screen.findByRole("table", { name: "Project files" });
    expect(within(table).getByRole("link", { name: "notes.txt" })).toHaveAttribute(
      "href", "/api/v1/projects/project-one/files/notes.txt",
    );
    expect(within(table).getByText("1.5 KiB")).toBeVisible();
    expect(screen.getByText("1 file · 1.5 KiB")).toBeVisible();
  });

  it("focuses the upload panel from Uploads navigation", async () => {
    const user = userEvent.setup();
    render(<App client={makeClient({ listProjects: vi.fn().mockResolvedValue([firstProject]) })} />);
    const navigation = await screen.findByRole("navigation", { name: "Workspace navigation" });

    await user.click(within(navigation).getByRole("button", { name: "Uploads" }));

    expect(screen.getByRole("region", { name: "Upload a file" })).toHaveFocus();
  });

  it("uses auto scrolling for Uploads navigation when reduced motion is preferred", async () => {
    const scrollIntoView = vi.fn();
    const originalScroll = HTMLElement.prototype.scrollIntoView;
    HTMLElement.prototype.scrollIntoView = scrollIntoView;
    vi.stubGlobal("matchMedia", vi.fn().mockReturnValue({ matches: true }));
    const user = userEvent.setup();
    render(<App client={makeClient({ listProjects: vi.fn().mockResolvedValue([firstProject]) })} />);
    const navigation = await screen.findByRole("navigation", { name: "Workspace navigation" });

    await user.click(within(navigation).getByRole("button", { name: "Uploads" }));

    expect(scrollIntoView).toHaveBeenCalledWith({ behavior: "auto", block: "center" });
    HTMLElement.prototype.scrollIntoView = originalScroll;
    vi.unstubAllGlobals();
  });

  it("refreshes the real file list only after upload completion", async () => {
    const uploaded: FileEntry = { name: "notes.bin", size: "10", modifiedAt: "2026-08-04T12:00:00Z" };
    const listFiles = vi.fn().mockResolvedValueOnce([]).mockResolvedValueOnce([uploaded]);
    const uploader = vi.fn(async (options) => ({
      name: options.file.name,
      size: String(options.file.size),
    }));
    const user = userEvent.setup();
    render(<App
      client={makeClient({ listProjects: vi.fn().mockResolvedValue([firstProject]), listFiles })}
      uploader={asUploader(uploader)}
    />);

    await screen.findByText("No files in this project yet.");
    expect(screen.getByText("No files")).toBeVisible();
    await user.upload(screen.getByLabelText("Choose a file to upload"), new File([new Uint8Array(10)], "notes.bin"));

    expect(await screen.findByRole("link", { name: "notes.bin" })).toBeVisible();
    expect(screen.getByText("1 file · 10 B")).toBeVisible();
    expect(listFiles).toHaveBeenCalledTimes(2);
  });
});
