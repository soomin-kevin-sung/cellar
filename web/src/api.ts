import type { CurrentUser, ErrorEnvelope, FileEntry, ManagedUser, Project, UserRole } from "./types";

const FALLBACK_MESSAGE = "문제가 발생했습니다. 다시 시도해주세요.";

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

async function ensureOk(response: Response): Promise<void> {
  if (response.ok) return;
  await readJson<never>(response);
}

export const api = {
  async me(signal?: AbortSignal): Promise<CurrentUser> {
    const response = await fetch("/api/v1/auth/me", { credentials: "same-origin", method: "GET", signal });
    return readJson<CurrentUser>(response);
  },

  async login(username: string, password: string, signal?: AbortSignal): Promise<CurrentUser> {
    const response = await fetch("/api/v1/auth/login", {
      body: JSON.stringify({ username: username.trim(), password }),
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      method: "POST",
      signal,
    });
    return readJson<CurrentUser>(response);
  },

  async logout(signal?: AbortSignal): Promise<void> {
    const response = await fetch("/api/v1/auth/logout", { credentials: "same-origin", method: "POST", signal });
    await ensureOk(response);
  },

  async listUsers(signal?: AbortSignal): Promise<ManagedUser[]> {
    const response = await fetch("/api/v1/admin/users", { credentials: "same-origin", method: "GET", signal });
    return readJson<ManagedUser[]>(response);
  },

  async createUser(username: string, password: string, role: UserRole, signal?: AbortSignal): Promise<void> {
    const response = await fetch("/api/v1/admin/users", {
      body: JSON.stringify({ username: username.trim(), password, role }),
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      method: "POST",
      signal,
    });
    await ensureOk(response);
  },

  async updateUser(userId: string, update: { active?: boolean; password?: string; role?: UserRole }, signal?: AbortSignal): Promise<void> {
    const response = await fetch(`/api/v1/admin/users/${encodeURIComponent(userId)}`, {
      body: JSON.stringify(update),
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      method: "PATCH",
      signal,
    });
    await ensureOk(response);
  },

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

};
