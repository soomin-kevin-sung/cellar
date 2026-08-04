import { api, ApiError } from "./api";

const project = {
  id: "a4e1557c-18d6-4e55-b037-9506f757f947",
  name: "Records",
  createdAt: "2026-08-04T10:00:00Z",
};

function jsonResponse(body: unknown, init: ResponseInit = {}) {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "Content-Type": "application/json" },
    ...init,
  });
}

describe("api", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("lists projects from the same-origin versioned endpoint", async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse([project]));
    vi.stubGlobal("fetch", fetchMock);

    await expect(api.listProjects()).resolves.toEqual([project]);
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/projects", {
      method: "GET",
      signal: undefined,
    });
  });

  it("creates a trimmed project with JSON headers and no manual Origin", async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(project, { status: 201 }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(api.createProject("  Records  ")).resolves.toEqual(project);
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/projects", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: "Records" }),
      signal: undefined,
    });
    const headers = fetchMock.mock.calls[0][1]?.headers as Record<string, string>;
    expect(headers).not.toHaveProperty("Origin");
  });

  it("lists files using an encoded project identifier", async () => {
    const files = [{ name: "notes.txt", size: "42", modifiedAt: "2026-08-04T10:00:00Z" }];
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(files));
    vi.stubGlobal("fetch", fetchMock);

    await expect(api.listFiles("project/id")).resolves.toEqual(files);
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/projects/project%2Fid/files", {
      method: "GET",
      signal: undefined,
    });
  });

  it("surfaces only the stable safe server message", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(
        {
          error: {
            code: "PROJECTS_UNAVAILABLE",
            message: "Projects are temporarily unavailable.",
            requestId: "private-request-id",
          },
        },
        { status: 503 },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    const error = await api.listProjects().catch((reason: unknown) => reason);

    expect(error).toBeInstanceOf(ApiError);
    expect(error).toHaveProperty("message", "Projects are temporarily unavailable.");
    expect(String(error)).not.toContain("private-request-id");
  });

  it.each([
    ["non-JSON", new Response("upstream exploded", { status: 502 })],
    ["malformed envelope", jsonResponse({ trace: "secret detail" }, { status: 500 })],
  ])("uses a safe fallback for a %s failure", async (_label, response) => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response));

    await expect(api.listProjects()).rejects.toThrow("Something went wrong. Please try again.");
  });
});
