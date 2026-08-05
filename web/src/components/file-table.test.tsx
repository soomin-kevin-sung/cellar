import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { formatBinarySize, formatFileSummary, formatModifiedTime } from "../file-format";
import { FileTable } from "./file-table";

describe("FileTable", () => {
  it("renders real file data with encoded same-origin download links", () => {
    render(<FileTable projectId="project/id" state="ready" files={[
      { name: "notes & plans.txt", size: "1536", modifiedAt: "2026-08-04T10:00:00Z" },
    ]} onRetry={vi.fn()} />);

    const table = screen.getByRole("table", { name: "프로젝트 파일" });
    expect(within(table).getByRole("columnheader", { name: "이름" })).toBeVisible();
    expect(within(table).getByRole("columnheader", { name: "크기" })).toBeVisible();
    expect(within(table).getByRole("columnheader", { name: "수정일" })).toBeVisible();
    expect(screen.getByRole("link", { name: "notes & plans.txt" })).toHaveAttribute(
      "href", "/api/v1/projects/project%2Fid/files/notes%20%26%20plans.txt",
    );
    expect(screen.getByText("1.5 KiB")).toBeVisible();
  });

  it("has loading, true empty, and retryable safe error states", async () => {
    const { rerender } = render(<FileTable projectId="one" state="loading" files={[]} onRetry={vi.fn()} />);
    expect(screen.getByRole("status")).toHaveTextContent("파일 불러오는 중");

    rerender(<FileTable projectId="one" state="ready" files={[]} onRetry={vi.fn()} />);
    expect(screen.getByText("아직 파일이 없습니다.")).toBeVisible();

    const onRetry = vi.fn();
    rerender(<FileTable projectId="one" state="error" files={[]} error="Files are unavailable." onRetry={onRetry} />);
    expect(screen.getByRole("alert")).toHaveTextContent("Files are unavailable.");
    await userEvent.click(screen.getByRole("button", { name: "파일 다시 불러오기" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it("uses safe raw fallbacks for malformed server values", () => {
    render(<FileTable projectId="one" state="ready" files={[
      { name: "odd.bin", size: "not-a-size", modifiedAt: "not-a-date" },
    ]} onRetry={vi.fn()} />);
    expect(screen.getByText("not-a-size")).toBeVisible();
    expect(screen.getByText("not-a-date")).toBeVisible();
  });

  it("filters by name and sorts the visible file set", async () => {
    const user = userEvent.setup();
    render(<FileTable projectId="one" state="ready" files={[
      { name: "alpha.txt", size: "10", modifiedAt: "2026-08-01T10:00:00Z" },
      { name: "beta.zip", size: "200", modifiedAt: "2026-08-04T10:00:00Z" },
    ]} onRetry={vi.fn()} />);

    await user.type(screen.getByRole("searchbox", { name: "파일 검색" }), "beta");
    expect(screen.getByRole("link", { name: "beta.zip" })).toBeVisible();
    expect(screen.queryByRole("link", { name: "alpha.txt" })).not.toBeInTheDocument();
    await user.clear(screen.getByRole("searchbox", { name: "파일 검색" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "파일 정렬" }), "largest");
    expect(screen.getAllByRole("link").map((link) => link.textContent)).toEqual(["beta.zip", "alpha.txt"]);
  });
});

describe("file formatting", () => {
  it.each([
    ["0", "0 B"], ["1024", "1 KiB"], ["1536", "1.5 KiB"], ["1048576", "1 MiB"],
  ])("formats %s bytes as %s", (raw, expected) => expect(formatBinarySize(raw)).toBe(expected));

  it("keeps unsafe decimal sizes raw", () => {
    expect(formatBinarySize("9007199254740992")).toBe("9007199254740992");
  });

  it("formats a valid timestamp and preserves an invalid timestamp", () => {
    expect(formatModifiedTime("2026-08-04T10:00:00Z")).not.toBe("2026-08-04T10:00:00Z");
    expect(formatModifiedTime("invalid")).toBe("invalid");
  });

  it("summarizes real file counts and sums decimal sizes without unsafe number coercion", () => {
    expect(formatFileSummary([
      { name: "one", size: "1024", modifiedAt: "" },
      { name: "two", size: "512", modifiedAt: "" },
    ])).toBe("2개 파일 · 1.5 KiB");
    expect(formatFileSummary([
      { name: "huge", size: "9007199254740992", modifiedAt: "" },
      { name: "one", size: "1", modifiedAt: "" },
    ])).toBe("2개 파일 · 9007199254740993 B");
  });
});
