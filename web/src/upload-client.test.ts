import {
  CHUNK_SIZE,
  STORAGE_KEY,
  UploadPausedError,
  UploadProtocolError,
  UploadRetryableError,
  clearStoredUpload,
  loadStoredUpload,
  matchesStoredUpload,
  saveStoredUpload,
  uploadFile,
  type StoredUpload,
} from "./upload-client";

const session = {
  id: "upload-one",
  projectId: "project-one",
  fileName: "archive.bin",
  totalSize: "10",
  committedOffset: "0",
  state: "active" as const,
};

function response(body: unknown, init: ResponseInit = {}) {
  return new Response(body === null ? null : JSON.stringify(body), {
    status: 200,
    headers: body === null ? undefined : { "Content-Type": "application/json" },
    ...init,
  });
}

function declaredFile(name = "archive.bin", size = 10) {
  const slices: Array<[number, number]> = [];
  const file = {
    name,
    size,
    slice(start: number, end: number) {
      slices.push([start, end]);
      return new Blob([new Uint8Array(end - start)]);
    },
  } as File;
  return { file, slices };
}

describe("uploadFile", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("uses the 32 MiB production chunk size", () => {
    expect(CHUNK_SIZE).toBe(32 * 1024 * 1024);
  });

  it("uses production chunk boundaries for a structural huge file without allocating its bytes", async () => {
    const total = CHUNK_SIZE * 2 + 7;
    const { file, slices } = declaredFile("large.bin", total);
    const largeSession = { ...session, fileName: "large.bin", totalSize: String(total) };
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(response(largeSession))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": String(CHUNK_SIZE) } }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": String(CHUNK_SIZE * 2) } }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": String(total) } }))
      .mockResolvedValueOnce(response({ ...largeSession, committedOffset: String(total), state: "complete" })));

    await uploadFile({ projectId: "project-one", file, session: largeSession });

    expect(slices).toEqual([[0, CHUNK_SIZE], [CHUNK_SIZE, CHUNK_SIZE * 2], [CHUNK_SIZE * 2, total]]);
  });

  it("creates, checks status, uploads exact sequential slices, and completes at total", async () => {
    const { file, slices } = declaredFile();
    const progress = vi.fn();
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(response(session, { status: 201 }))
      .mockResolvedValueOnce(response(session))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "4" } }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "8" } }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "10" } }))
      .mockResolvedValueOnce(response({ ...session, committedOffset: "10", state: "complete" }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(uploadFile({ projectId: "project-one", file, chunkSize: 4, onProgress: progress }))
      .resolves.toMatchObject({ state: "complete", committedOffset: "10" });

    expect(fetchMock.mock.calls[0]).toEqual([
      "/api/v1/projects/project-one/uploads",
      expect.objectContaining({
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ fileName: "archive.bin", totalSize: "10" }),
      }),
    ]);
    expect(fetchMock.mock.calls[1][0]).toBe("/api/v1/uploads/upload-one");
    expect(slices).toEqual([[0, 4], [4, 8], [8, 10]]);
    const puts = fetchMock.mock.calls.slice(2, 5);
    expect(puts.map((call) => call[1]?.headers)).toEqual([
      { "Content-Type": "application/octet-stream", "Upload-Offset": "0" },
      { "Content-Type": "application/octet-stream", "Upload-Offset": "4" },
      { "Content-Type": "application/octet-stream", "Upload-Offset": "8" },
    ]);
    for (const [, init] of puts) {
      expect(init?.headers).not.toHaveProperty("Content-Length");
      expect(init?.headers).not.toHaveProperty("Origin");
    }
    expect(fetchMock.mock.calls[5][0]).toBe("/api/v1/uploads/upload-one/complete");
    expect(progress).toHaveBeenLastCalledWith(expect.objectContaining({ committed: 10, total: 10 }));
  });

  it("adopts an authoritative forward jump from an idempotent earlier retry", async () => {
    const { file, slices } = declaredFile();
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(response({ ...session, committedOffset: "4" }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "8" } }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "10" } }))
      .mockResolvedValueOnce(response({ ...session, committedOffset: "10", state: "complete" }));
    vi.stubGlobal("fetch", fetchMock);

    await uploadFile({ projectId: "project-one", file, session, chunkSize: 4 });

    expect(slices).toEqual([[4, 8], [8, 10]]);
    expect(fetchMock).toHaveBeenCalledTimes(4);
  });

  it("creates and completes a zero-byte upload without chunk requests", async () => {
    const { file, slices } = declaredFile("empty.bin", 0);
    const emptySession = { ...session, fileName: "empty.bin", totalSize: "0" };
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(response(emptySession, { status: 201 }))
      .mockResolvedValueOnce(response(emptySession))
      .mockResolvedValueOnce(response({ ...emptySession, state: "complete" }));
    vi.stubGlobal("fetch", fetchMock);

    await uploadFile({ projectId: "project-one", file });

    expect(slices).toEqual([]);
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it.each([
    ["missing", null],
    ["malformed", "wat"],
    ["decreasing", "3"],
    ["out of bounds", "11"],
  ])("fails safely for a %s authoritative chunk offset", async (_label, offset) => {
    const { file } = declaredFile();
    const headers = offset === null ? undefined : { "Upload-Offset": offset };
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(response({ ...session, committedOffset: "4" }))
      .mockResolvedValueOnce(response(null, { status: 204, headers })));

    await expect(uploadFile({ projectId: "project-one", file, session, chunkSize: 4 }))
      .rejects.toBeInstanceOf(UploadProtocolError);
  });

  it("adopts a stable 409 expectedOffset and continues without looping", async () => {
    const { file, slices } = declaredFile();
    const conflict = response({ error: { code: "upload_offset_conflict", message: "Conflict", requestId: "private", details: { expectedOffset: "8" } } }, { status: 409 });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ ...session, committedOffset: "4" }))
      .mockResolvedValueOnce(conflict)
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "10" } }))
      .mockResolvedValueOnce(response({ ...session, committedOffset: "10", state: "complete" }));
    vi.stubGlobal("fetch", fetchMock);

    await uploadFile({ projectId: "project-one", file, session, chunkSize: 4 });

    expect(slices).toEqual([[4, 8], [8, 10]]);
    expect(fetchMock).toHaveBeenCalledTimes(4);
  });

  it.each([503, 507, 429])("keeps a chunk %s retryable and resumes the same session without recreate", async (status) => {
    const { file } = declaredFile();
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response(session))
      .mockResolvedValueOnce(response({ error: { message: "Try again later." } }, { status }))
      .mockResolvedValueOnce(response(session))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "10" } }))
      .mockResolvedValueOnce(response({ ...session, committedOffset: "10", state: "complete" }));
    vi.stubGlobal("fetch", fetchMock);

    const paused = await uploadFile({ projectId: "project-one", file, session, chunkSize: 10 })
      .catch((error: unknown) => error);
    expect(paused).toBeInstanceOf(UploadRetryableError);
    expect(paused).toHaveProperty("session.committedOffset", "0");
    if (!(paused instanceof UploadRetryableError)) throw new Error("Expected retryable chunk");

    await expect(uploadFile({ projectId: "project-one", file, session: paused.session, chunkSize: 10 }))
      .resolves.toMatchObject({ state: "complete", committedOffset: "10" });
    expect(fetchMock.mock.calls.some((call) => String(call[0]).includes("/projects/"))).toBe(false);
    expect(fetchMock.mock.calls.filter((call) => call[1]?.method === "PUT")).toHaveLength(2);
  });

  it.each([413, 404])("treats a chunk %s as terminal with the safe server message", async (status) => {
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(response(session))
      .mockResolvedValueOnce(response({ error: {
        code: "chunk_rejected", message: "This chunk cannot be accepted.", requestId: "private-id",
      } }, { status })));

    const result = await uploadFile({ projectId: "project-one", file, session, chunkSize: 10 })
      .catch((error: unknown) => error);
    expect(result).toBeInstanceOf(UploadProtocolError);
    expect(result).not.toBeInstanceOf(UploadRetryableError);
    expect(result).toHaveProperty("message", "This chunk cannot be accepted.");
    expect(String(result)).not.toContain("private-id");
  });

  it("pauses on a network failure and resumes the same session without recreating", async () => {
    const { file } = declaredFile();
    const firstFetch = vi.fn()
      .mockResolvedValueOnce(response(session, { status: 201 }))
      .mockResolvedValueOnce(response(session))
      .mockRejectedValueOnce(new TypeError("offline"));
    vi.stubGlobal("fetch", firstFetch);

    const paused = await uploadFile({ projectId: "project-one", file, chunkSize: 4 }).catch((error: unknown) => error);
    expect(paused).toBeInstanceOf(UploadPausedError);
    expect(paused).toHaveProperty("session.id", "upload-one");
    if (!(paused instanceof UploadPausedError)) throw new Error("Expected a paused upload");

    const secondFetch = vi.fn()
      .mockResolvedValueOnce(response({ ...session, committedOffset: "4" }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "8" } }))
      .mockResolvedValueOnce(response(null, { status: 204, headers: { "Upload-Offset": "10" } }))
      .mockResolvedValueOnce(response({ ...session, committedOffset: "10", state: "complete" }));
    vi.stubGlobal("fetch", secondFetch);

    await uploadFile({ projectId: "project-one", file, session: paused.session, chunkSize: 4 });
    expect(secondFetch.mock.calls[0][0]).toBe("/api/v1/uploads/upload-one");
    expect(secondFetch.mock.calls.some((call) => String(call[0]).includes("/projects/"))).toBe(false);
  });

  it("propagates AbortError instead of converting cancellation to a pause", async () => {
    const controller = new AbortController();
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn((_url: string, init: RequestInit) => {
      controller.abort();
      return Promise.reject(init.signal?.reason ?? new DOMException("Aborted", "AbortError"));
    }));

    await expect(uploadFile({ projectId: "project-one", file, signal: controller.signal }))
      .rejects.toHaveProperty("name", "AbortError");
  });

  it("short-circuits when authoritative status is already complete", async () => {
    const { file, slices } = declaredFile();
    const complete = { ...session, committedOffset: "10", state: "complete" as const };
    const fetchMock = vi.fn().mockResolvedValueOnce(response(complete));
    vi.stubGlobal("fetch", fetchMock);

    await expect(uploadFile({ projectId: "project-one", file, session })).resolves.toEqual(complete);
    expect(slices).toEqual([]);
    expect(fetchMock).toHaveBeenCalledOnce();
  });

  it("retries transient completion and finalizing status with one session until authoritative complete", async () => {
    localStorage.clear();
    const { file } = declaredFile();
    const ready = { ...session, committedOffset: "10" };
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response(ready))
      .mockResolvedValueOnce(response({ error: { message: "Temporarily unavailable" } }, { status: 503 }))
      .mockResolvedValueOnce(response({ ...ready, state: "finalizing" }))
      .mockResolvedValueOnce(response({ ...ready, state: "complete" }));
    vi.stubGlobal("fetch", fetchMock);

    const transient = await uploadFile({ projectId: "project-one", file, session: ready })
      .catch((error: unknown) => error);
    expect(transient).toBeInstanceOf(UploadRetryableError);
    if (!(transient instanceof UploadRetryableError)) throw new Error("Expected retryable completion");
    expect(localStorage.getItem(STORAGE_KEY)).not.toBeNull();

    const finalizing = await uploadFile({ projectId: "project-one", file, session: transient.session })
      .catch((error: unknown) => error);
    expect(finalizing).toBeInstanceOf(UploadRetryableError);
    if (!(finalizing instanceof UploadRetryableError)) throw new Error("Expected retryable finalization");

    await expect(uploadFile({ projectId: "project-one", file, session: finalizing.session }))
      .resolves.toMatchObject({ state: "complete" });
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(fetchMock).toHaveBeenCalledTimes(4);
    expect(fetchMock.mock.calls.filter((call) => call[1]?.method === "POST")).toHaveLength(1);
    expect(fetchMock.mock.calls.some((call) => String(call[0]).includes("/projects/"))).toBe(false);
  });

  it("treats malformed successful status JSON as a terminal protocol failure", async () => {
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(new Response("not-json", { status: 200 })));

    const result = await uploadFile({ projectId: "project-one", file, session }).catch((error: unknown) => error);
    expect(result).toBeInstanceOf(UploadProtocolError);
    expect(result).not.toBeInstanceOf(UploadRetryableError);
  });

  it("treats a 404 status as terminal with only the safe server message", async () => {
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(response({ error: {
      code: "upload_not_found", message: "Upload not found.", requestId: "private-id",
    } }, { status: 404 })));

    const result = await uploadFile({ projectId: "project-one", file, session }).catch((error: unknown) => error);
    expect(result).toBeInstanceOf(UploadProtocolError);
    expect(result).not.toBeInstanceOf(UploadRetryableError);
    expect(result).toHaveProperty("message", "Upload not found.");
    expect(String(result)).not.toContain("private-id");
  });

  it("keeps a 503 status retryable with the existing session", async () => {
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(response({ error: {
      message: "Temporarily unavailable.",
    } }, { status: 503 })));

    const result = await uploadFile({ projectId: "project-one", file, session }).catch((error: unknown) => error);
    expect(result).toBeInstanceOf(UploadRetryableError);
    expect(result).toHaveProperty("session.id", "upload-one");
  });

  it("rechecks a completion 409 and treats an authoritative active state as terminal", async () => {
    const { file } = declaredFile();
    const ready = { ...session, committedOffset: "10" };
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response(ready))
      .mockResolvedValueOnce(response({ error: { message: "Conflict" } }, { status: 409 }))
      .mockResolvedValueOnce(response(ready));
    vi.stubGlobal("fetch", fetchMock);

    const result = await uploadFile({ projectId: "project-one", file, session: ready }).catch((error: unknown) => error);
    expect(result).toBeInstanceOf(UploadProtocolError);
    expect(result).not.toBeInstanceOf(UploadRetryableError);
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("rechecks a completion 409, preserves finalizing, and later accepts authoritative complete", async () => {
    localStorage.clear();
    const { file } = declaredFile();
    const ready = { ...session, committedOffset: "10" };
    const finalizing = { ...ready, state: "finalizing" as const };
    const complete = { ...ready, state: "complete" as const };
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response(ready))
      .mockResolvedValueOnce(response({ error: { message: "Conflict" } }, { status: 409 }))
      .mockResolvedValueOnce(response(finalizing))
      .mockResolvedValueOnce(response(complete));
    vi.stubGlobal("fetch", fetchMock);

    const pending = await uploadFile({ projectId: "project-one", file, session: ready }).catch((error: unknown) => error);
    expect(pending).toBeInstanceOf(UploadRetryableError);
    if (!(pending instanceof UploadRetryableError)) throw new Error("Expected finalizing retry");
    await expect(uploadFile({ projectId: "project-one", file, session: pending.session })).resolves.toEqual(complete);
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(fetchMock.mock.calls.some((call) => String(call[0]).includes("/projects/"))).toBe(false);
  });

  it("treats a finalizing status below total as fatal protocol corruption", async () => {
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(response({
      ...session, committedOffset: "9", state: "finalizing",
    })));

    const result = await uploadFile({ projectId: "project-one", file, session }).catch((error: unknown) => error);
    expect(result).toBeInstanceOf(UploadProtocolError);
    expect(result).not.toBeInstanceOf(UploadRetryableError);
  });

  it("rejects a complete status whose committed offset is not total", async () => {
    const { file } = declaredFile();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(response({
      ...session, committedOffset: "9", state: "complete",
    })));

    await expect(uploadFile({ projectId: "project-one", file, session })).rejects.toBeInstanceOf(UploadProtocolError);
  });

  it("rejects unsafe file sizes before a request", async () => {
    const { file } = declaredFile("huge.bin", Number.MAX_SAFE_INTEGER + 1);
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);

    await expect(uploadFile({ projectId: "project-one", file })).rejects.toBeInstanceOf(UploadProtocolError);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe("upload recovery persistence", () => {
  const stored: StoredUpload = {
    uploadId: "upload-one",
    projectId: "project-one",
    fileName: "archive.bin",
    totalSize: "10",
    committedOffset: "4",
  };

  beforeEach(() => localStorage.clear());

  it("stores only the exact recovery schema", () => {
    saveStoredUpload(stored);
    expect(JSON.parse(localStorage.getItem(STORAGE_KEY)!)).toEqual(stored);
    expect(loadStoredUpload()).toEqual(stored);
  });

  it("strips unexpected runtime fields from new writes", () => {
    saveStoredUpload({ ...stored, token: "secret" } as StoredUpload & { token: string });
    expect(JSON.parse(localStorage.getItem(STORAGE_KEY)!)).toEqual(stored);
  });

  it("rejects malformed stored metadata and supports explicit clearing", () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...stored, token: "secret" }));
    expect(loadStoredUpload()).toBeNull();
    clearStoredUpload();
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("rejects legacy versioned metadata instead of widening the persisted schema", () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: 1, ...stored }));
    expect(loadStoredUpload()).toBeNull();
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("removes invalid metadata best-effort so a recovery prompt does not recur", () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...stored, token: "secret" }));
    expect(loadStoredUpload()).toBeNull();
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("returns null when browser storage access is disabled", () => {
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new DOMException("Disabled"); });
    expect(loadStoredUpload()).toBeNull();
    getItem.mockRestore();
  });

  it("matches only the exact project, name, and size", () => {
    expect(matchesStoredUpload(stored, "project-one", declaredFile().file)).toBe(true);
    expect(matchesStoredUpload(stored, "project-two", declaredFile().file)).toBe(false);
    expect(matchesStoredUpload(stored, "project-one", declaredFile("other.bin").file)).toBe(false);
    expect(matchesStoredUpload(stored, "project-one", declaredFile("archive.bin", 11).file)).toBe(false);
  });
});
