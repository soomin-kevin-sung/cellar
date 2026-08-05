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

  it("uploads a file in server-sized chunks and reports committed progress", async () => {
    const file = new File(["cellar"], "notes 한글.txt");
    const progress = vi.fn();
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse({ uploadId: "upload-one", chunkSize: "4", offset: "0" }, { status: 201 }))
      .mockResolvedValueOnce(jsonResponse({ offset: "4" }))
      .mockResolvedValueOnce(jsonResponse({ offset: String(file.size) }))
      .mockResolvedValueOnce(jsonResponse({ name: file.name, size: String(file.size) }, { status: 201 }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(uploadFile({ projectId: "project/id", file, onProgress: progress })).resolves.toEqual({
      name: file.name,
      size: String(file.size),
    });
    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      "/api/v1/projects/project%2Fid/upload-sessions",
      expect.objectContaining({
        method: "POST",
        headers: { "Content-Type": "application/json" },
      }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/v1/upload-sessions/upload-one/chunks?offset=0",
      expect.objectContaining({ method: "PUT" }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      3,
      "/api/v1/upload-sessions/upload-one/chunks?offset=4",
      expect.objectContaining({ method: "PUT" }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      4,
      "/api/v1/upload-sessions/upload-one/complete",
      expect.objectContaining({ method: "POST" }),
    );
    expect(progress.mock.calls).toEqual([[0, file.size], [4, file.size], [file.size, file.size]]);
  });

  it("discards server staging when a chunk request fails", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse({ uploadId: "upload-two", chunkSize: "4", offset: "0" }, { status: 201 }))
      .mockRejectedValueOnce(new TypeError("connection lost"))
      .mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(uploadFile({ projectId: "project", file: new File(["cellar"], "notes.txt") }))
      .rejects.toThrow("파일을 업로드하지 못했습니다. 다시 시도해주세요.");
    expect(fetchMock).toHaveBeenNthCalledWith(
      3,
      "/api/v1/upload-sessions/upload-two",
      { method: "DELETE" },
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
      .rejects.toThrow("파일을 업로드하지 못했습니다. 다시 시도해주세요.");
  });
});
