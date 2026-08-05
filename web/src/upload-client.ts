export interface UploadResult {
  name: string;
  size: string;
}

export interface UploadOptions {
  projectId: string;
  file: File;
  onProgress?: (uploadedBytes: number, totalBytes: number) => void;
  signal?: AbortSignal;
}

export class UploadError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = "UploadError";
  }
}

const FALLBACK_MESSAGE = "파일을 업로드하지 못했습니다. 다시 시도해주세요.";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function errorMessage(response: Response) {
  try {
    const body: unknown = await response.json();
    if (isRecord(body) && isRecord(body.error) && typeof body.error.message === "string" && body.error.message.trim()) {
      return body.error.message;
    }
  } catch {
    // Proxies may return HTML or an empty response. Keep the stable fallback.
  }
  return FALLBACK_MESSAGE;
}

function isAbort(reason: unknown, signal?: AbortSignal) {
  return signal?.aborted || (reason instanceof DOMException && reason.name === "AbortError") ||
    (reason instanceof Error && reason.name === "AbortError");
}

async function request(url: string, init: RequestInit, signal?: AbortSignal) {
  let response: Response;
  try {
    response = await fetch(url, { ...init, signal });
  } catch (reason) {
    if (isAbort(reason, signal)) throw reason;
    throw new UploadError(FALLBACK_MESSAGE, 0);
  }
  if (!response.ok) throw new UploadError(await errorMessage(response), response.status);
  return response;
}

async function responseBody(response: Response) {
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new UploadError(FALLBACK_MESSAGE, response.status);
  }
  if (!isRecord(body)) throw new UploadError(FALLBACK_MESSAGE, response.status);
  return body;
}

function decimal(value: unknown) {
  if (typeof value !== "string" || !/^\d+$/.test(value)) return null;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : null;
}

async function discardUpload(uploadId: string) {
  try {
    await fetch(`/api/v1/upload-sessions/${encodeURIComponent(uploadId)}`, { method: "DELETE" });
  } catch {
    // The server also removes unfinished staging files at startup.
  }
}

export async function uploadFile({ projectId, file, onProgress, signal }: UploadOptions): Promise<UploadResult> {
  let uploadId: string | null = null;
  try {
    const started = await request(
      `/api/v1/projects/${encodeURIComponent(projectId)}/upload-sessions`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ fileName: file.name, totalSize: String(file.size) }),
      },
      signal,
    );
    const startBody = await responseBody(started);
    const chunkSize = decimal(startBody.chunkSize);
    const initialOffset = decimal(startBody.offset);
    if (typeof startBody.uploadId !== "string" || !startBody.uploadId || !chunkSize || initialOffset !== 0) {
      throw new UploadError(FALLBACK_MESSAGE, started.status);
    }
    uploadId = startBody.uploadId;
    onProgress?.(0, file.size);

    let offset = 0;
    while (offset < file.size) {
      const nextOffset = Math.min(offset + chunkSize, file.size);
      const chunkResponse = await request(
        `/api/v1/upload-sessions/${encodeURIComponent(uploadId)}/chunks?offset=${offset}`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/octet-stream" },
          body: file.slice(offset, nextOffset),
        },
        signal,
      );
      const chunkBody = await responseBody(chunkResponse);
      if (decimal(chunkBody.offset) !== nextOffset) {
        throw new UploadError(FALLBACK_MESSAGE, chunkResponse.status);
      }
      offset = nextOffset;
      onProgress?.(offset, file.size);
    }

    const completed = await request(
      `/api/v1/upload-sessions/${encodeURIComponent(uploadId)}/complete`,
      { method: "POST" },
      signal,
    );
    const body = await responseBody(completed);
    uploadId = null;
    if (body.name !== file.name || body.size !== String(file.size)) {
      throw new UploadError(FALLBACK_MESSAGE, completed.status);
    }
    return { name: body.name, size: body.size };
  } catch (reason) {
    if (uploadId) await discardUpload(uploadId);
    throw reason;
  }
}
