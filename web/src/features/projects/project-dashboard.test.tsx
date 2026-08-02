import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { ProjectDashboard } from "./project-dashboard";

describe("ProjectDashboard", () => {
  it("presents project and recent-file content as the first useful screen", () => {
    render(<ProjectDashboard />);

    expect(screen.getByRole("heading", { name: "프로젝트 보관함" })).toBeVisible();
    expect(screen.getByRole("region", { name: "프로젝트 목록" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "최근 파일" })).toBeVisible();
    expect(screen.getByText("Access 연동 전")).toBeVisible();
  });

  it("filters visible projects by project metadata", async () => {
    const user = userEvent.setup();
    render(<ProjectDashboard />);

    await user.type(screen.getByRole("searchbox", { name: "프로젝트 검색" }), "archive");

    expect(screen.getByRole("heading", { name: "Archive 2025" })).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Studio References" })).not.toBeInTheDocument();
  });
});
