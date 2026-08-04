export interface UploadResult {
  name: string;
  size: string;
}

export interface UploadOptions {
  projectId: string;
  file: File;
  signal?: AbortSignal;
}

export class UploadError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = "UploadError";
  }
}

const FALLBACK_MESSAGE = "The file could not be uploaded. Please try again.";

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

export async function uploadFile({ projectId, file, signal }: UploadOptions): Promise<UploadResult> {
  let response: Response;
  try {
    response = await fetch(
      `/api/v1/projects/${encodeURIComponent(projectId)}/uploads?fileName=${encodeURIComponent(file.name)}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: file,
        signal,
      },
    );
  } catch (reason) {
    if (isAbort(reason, signal)) throw reason;
    throw new UploadError(FALLBACK_MESSAGE, 0);
  }

  if (!response.ok) throw new UploadError(await errorMessage(response), response.status);

  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new UploadError(FALLBACK_MESSAGE, response.status);
  }
  if (!isRecord(body) || body.name !== file.name || body.size !== String(file.size)) {
    throw new UploadError(FALLBACK_MESSAGE, response.status);
  }
  return { name: body.name, size: body.size };
}
