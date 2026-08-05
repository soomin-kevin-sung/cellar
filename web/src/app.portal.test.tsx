import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import App, { type ApiClient } from "./app";

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

  it("routes project, upload, and admin features through the shared portal", async () => {
    vi.stubGlobal("matchMedia", vi.fn().mockReturnValue({ matches: true }));
    const random = vi.spyOn(Math, "random").mockReturnValue(0.5);
    const user = userEvent.setup();
    render(<App client={client} currentUser={{ id: "admin", username: "cellar", role: "admin" }} />);

    await user.click(await screen.findByRole("button", { name: "Records" }));
    expect(document.querySelector(".feature-portal--open .project-workspace")).toBeInTheDocument();
    const randomCalls = random.mock.calls.length;
    await user.click(screen.getByRole("button", { name: "Records" }));
    expect(random).toHaveBeenCalledTimes(randomCalls);
    expect(document.querySelector(".feature-portal--open .project-workspace")).toBeInTheDocument();

    const navigation = screen.getByRole("navigation", { name: "작업 메뉴" });
    await user.click(within(navigation).getByRole("button", { name: "업로드" }));
    expect(document.querySelector(".feature-portal--open .upload-workspace")).toBeInTheDocument();

    await user.click(within(navigation).getByRole("button", { name: "사용자" }));
    expect(document.querySelector(".feature-portal--open .admin-page")).toBeInTheDocument();
    random.mockRestore();
    vi.unstubAllGlobals();
  });
});
