import type { ErrorEnvelope, FileEntry, Project, UploadSession } from "./types";

const FALLBACK_MESSAGE = "Something went wrong. Please try again.";

export class ApiError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = "ApiError";
  }
}

function isErrorEnvelope(value: unknown): value is ErrorEnvelope {
  if (typeof value !== "object" || value === null || !("error" in value)) return false;
  const error = value.error;
  return (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string" &&
    error.message.trim().length > 0
  );
}

async function readJson<T>(response: Response): Promise<T> {
  if (!response.ok) {
    let message = FALLBACK_MESSAGE;
    try {
      const body: unknown = await response.json();
      if (isErrorEnvelope(body)) message = body.error.message;
    } catch {
      // A proxy or interrupted server may not return JSON. Keep the safe fallback.
    }
    throw new ApiError(message, response.status);
  }

  try {
    return (await response.json()) as T;
  } catch {
    throw new ApiError(FALLBACK_MESSAGE, response.status);
  }
}

export const api = {
  async listProjects(signal?: AbortSignal): Promise<Project[]> {
    const response = await fetch("/api/v1/projects", { method: "GET", signal });
    return readJson<Project[]>(response);
  },

  async createProject(name: string, signal?: AbortSignal): Promise<Project> {
    const response = await fetch("/api/v1/projects", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: name.trim() }),
      signal,
    });
    return readJson<Project>(response);
  },

  async listFiles(projectId: string, signal?: AbortSignal): Promise<FileEntry[]> {
    const response = await fetch(`/api/v1/projects/${encodeURIComponent(projectId)}/files`, {
      method: "GET",
      signal,
    });
    return readJson<FileEntry[]>(response);
  },

  async createUpload(projectId: string, fileName: string, totalSize: number, signal?: AbortSignal): Promise<UploadSession> {
    if (!Number.isSafeInteger(totalSize) || totalSize < 0) {
      throw new ApiError("The selected file is too large for this browser.", 0);
    }
    const response = await fetch(`/api/v1/projects/${encodeURIComponent(projectId)}/uploads`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ fileName, totalSize: String(totalSize) }),
      signal,
    });
    return readJson<UploadSession>(response);
  },

  async getUpload(uploadId: string, signal?: AbortSignal): Promise<UploadSession> {
    const response = await fetch(`/api/v1/uploads/${encodeURIComponent(uploadId)}`, { method: "GET", signal });
    return readJson<UploadSession>(response);
  },

  async completeUpload(uploadId: string, signal?: AbortSignal): Promise<UploadSession> {
    const response = await fetch(`/api/v1/uploads/${encodeURIComponent(uploadId)}/complete`, { method: "POST", signal });
    return readJson<UploadSession>(response);
  },
};
