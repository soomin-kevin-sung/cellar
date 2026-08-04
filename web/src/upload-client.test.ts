import { UploadError, uploadFile } from "./upload-client";

function jsonResponse(body: unknown, init: ResponseInit = {}) {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "Content-Type": "application/json" },
    ...init,
  });
}

describe("uploadFile", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("uploads the complete file in one request", async () => {
    const file = new File(["cellar"], "notes 한글.txt");
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({ name: file.name, size: String(file.size) }, { status: 201 }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(uploadFile({ projectId: "project/id", file })).resolves.toEqual({
      name: file.name,
      size: String(file.size),
    });
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/v1/projects/project%2Fid/uploads?fileName=${encodeURIComponent(file.name)}`,
      expect.objectContaining({
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: file,
      }),
    );
  });

  it("surfaces the safe server error message", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse({
      error: { code: "file_conflict", message: "A file with this name already exists.", requestId: "private" },
    }, { status: 409 })));

    const error = await uploadFile({ projectId: "project", file: new File(["x"], "same.txt") })
      .catch((reason: unknown) => reason);

    expect(error).toBeInstanceOf(UploadError);
    expect(error).toHaveProperty("status", 409);
    expect(error).toHaveProperty("message", "A file with this name already exists.");
    expect(String(error)).not.toContain("private");
  });

  it("uses a stable fallback for network or malformed responses", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("private network detail")));
    await expect(uploadFile({ projectId: "project", file: new File(["x"], "file.txt") }))
      .rejects.toThrow("The file could not be uploaded. Please try again.");
  });
});
