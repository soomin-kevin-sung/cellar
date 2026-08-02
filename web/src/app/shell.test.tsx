import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { AppShell } from "./shell";

describe("AppShell", () => {
  it("provides labeled navigation and a visible main landmark", () => {
    render(
      <AppShell>
        <p>Archive content</p>
      </AppShell>,
    );

    expect(screen.getByRole("navigation", { name: "주 탐색" })).toBeVisible();
    expect(screen.getByRole("main")).toHaveTextContent("Archive content");
    expect(screen.getByText("UI preview")).toBeVisible();
  });

  it("opens mobile navigation from a labeled control", async () => {
    const user = userEvent.setup();
    render(
      <AppShell>
        <p>Archive content</p>
      </AppShell>,
    );

    await user.click(screen.getByRole("button", { name: "메뉴 열기" }));

    expect(screen.getByRole("navigation", { name: "모바일 탐색" })).toBeVisible();
    expect(screen.getByRole("button", { name: "메뉴 닫기" })).toBeVisible();
  });
});
