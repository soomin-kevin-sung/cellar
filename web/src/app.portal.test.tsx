import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import App, { type ApiClient } from "./app";
import type { uploadFile } from "./upload-client";

const project = { id: "project-one", name: "Records", createdAt: "2026-08-05T00:00:00Z" };

const client: ApiClient = {
  listProjects: vi.fn().mockResolvedValue([project]),
  createProject: vi.fn().mockResolvedValue(project),
  listFiles: vi.fn().mockResolvedValue([]),
};

describe("App feature portal", () => {
  it("shows the idle cube even before the first project exists", async () => {
    render(<App client={{ ...client, listProjects: vi.fn().mockResolvedValue([]) }} />);

    await waitFor(() => expect(document.querySelector(".feature-portal--idle .feature-portal__cube")).toBeInTheDocument());
    expect(screen.getAllByRole("button", { name: "프로젝트 만들기" }).length).toBeGreaterThan(0);
  });

  it("opens a project home before the file browser", async () => {
    vi.stubGlobal("matchMedia", vi.fn().mockReturnValue({ matches: true }));
    const user = userEvent.setup();
    render(<App client={client} />);

    await user.click(await screen.findByRole("button", { name: "Records" }));
    expect(document.querySelector(".feature-portal--open .project-home")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Records" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "프로젝트 만들기" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "파일 탐색" }));
    expect(document.querySelector(".feature-portal--open .project-workspace")).toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  it("routes project, upload, and admin features through the shared portal", async () => {
    vi.stubGlobal("matchMedia", vi.fn().mockReturnValue({ matches: true }));
    const random = vi.spyOn(Math, "random").mockReturnValue(0.5);
    const user = userEvent.setup();
    render(<App client={client} currentUser={{ id: "admin", username: "cellar", role: "admin" }} />);

    await user.click(await screen.findByRole("button", { name: "Records" }));
    expect(document.querySelector(".feature-portal--open .project-home")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "프로젝트 만들기" })).not.toBeInTheDocument();
    const randomCalls = random.mock.calls.length;
    await user.click(screen.getByRole("button", { name: "Records" }));
    expect(random).toHaveBeenCalledTimes(randomCalls);
    expect(document.querySelector(".feature-portal--open .project-home")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "파일 탐색" }));
    expect(document.querySelector(".feature-portal--open .project-workspace")).toBeInTheDocument();

    const navigation = screen.getByRole("navigation", { name: "작업 메뉴" });
    await user.click(within(navigation).getByRole("button", { name: "업로드" }));
    expect(document.querySelector(".feature-portal--open .upload-workspace")).toBeInTheDocument();
    const uploadRandomCalls = random.mock.calls.length;
    await user.click(within(navigation).getByRole("button", { name: "업로드" }));
    expect(random).toHaveBeenCalledTimes(uploadRandomCalls);
    expect(document.querySelector(".feature-portal--open .upload-workspace")).toBeInTheDocument();

    await user.click(within(navigation).getByRole("button", { name: "사용자" }));
    expect(document.querySelector(".feature-portal--open .admin-page")).toBeInTheDocument();
    const adminRandomCalls = random.mock.calls.length;
    await user.click(within(navigation).getByRole("button", { name: "사용자" }));
    expect(random).toHaveBeenCalledTimes(adminRandomCalls);
    expect(document.querySelector(".feature-portal--open .admin-page")).toBeInTheDocument();
    random.mockRestore();
    vi.unstubAllGlobals();
  });

  it("keeps an upload alive while navigating to another feature", async () => {
    vi.stubGlobal("matchMedia", vi.fn().mockReturnValue({ matches: true }));
    let finish!: (value: { name: string; size: string }) => void;
    const uploader = vi.fn((options: Parameters<typeof uploadFile>[0]) => {
      options.onProgress?.(3, 6);
      return new Promise<{ name: string; size: string }>((resolve) => { finish = resolve; });
    }) as unknown as typeof uploadFile;
    const user = userEvent.setup();
    render(<App
      client={client}
      currentUser={{ id: "admin", username: "cellar", role: "admin" }}
      uploader={uploader}
    />);

    await user.click(await screen.findByRole("button", { name: "Records" }));
    await user.click(screen.getByRole("button", { name: "파일 탐색" }));
    const file = new File(["cellar"], "archive.bin");
    await user.upload(screen.getByLabelText("현재 프로젝트에 파일 업로드"), file);
    expect(screen.getByLabelText("백그라운드 업로드")).toHaveTextContent("50%");

    const navigation = screen.getByRole("navigation", { name: "작업 메뉴" });
    await user.click(within(navigation).getByRole("button", { name: "사용자" }));
    expect(document.querySelector(".feature-portal--open .admin-page")).toBeInTheDocument();
    expect(screen.getByLabelText("백그라운드 업로드")).toHaveTextContent("archive.bin");

    await act(async () => finish({ name: file.name, size: String(file.size) }));
    expect(screen.getByLabelText("백그라운드 업로드")).toHaveTextContent("저장됨");
    vi.unstubAllGlobals();
  });
});
