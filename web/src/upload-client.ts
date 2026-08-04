import type { UploadSession, UploadState } from "./types";
export type { UploadSession } from "./types";

export const CHUNK_SIZE = 32 * 1024 * 1024;
export const STORAGE_KEY = "cellar.upload.v1";

export interface StoredUpload {
  uploadId: string;
  projectId: string;
  fileName: string;
  totalSize: string;
  committedOffset: string;
}

export interface UploadProgress {
  committed: number;
  total: number;
  session: UploadSession;
}

export class UploadProtocolError extends Error {
  constructor(message = "The upload could not continue safely.") {
    super(message);
    this.name = "UploadProtocolError";
  }
}

export class UploadPausedError extends Error {
  constructor(readonly session: UploadSession) {
    super("The upload is paused because the connection was interrupted.");
    this.name = "UploadPausedError";
  }
}

export class UploadRetryableError extends Error {
  constructor(readonly session: UploadSession, message = "The upload is not finished yet. Please retry.") {
    super(message);
    this.name = "UploadRetryableError";
  }
}

class MalformedUploadResponse extends Error {}

const SAFE_FALLBACK = "The upload could not continue safely.";

interface UploadOptions {
  projectId: string;
  file: File;
  session?: UploadSession;
  signal?: AbortSignal;
  onProgress?: (progress: UploadProgress) => void;
  chunkSize?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseDecimal(value: unknown, maximum = Number.MAX_SAFE_INTEGER): number {
  if (typeof value !== "string" || !/^(0|[1-9]\d*)$/.test(value)) throw new UploadProtocolError();
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0 || parsed > maximum) throw new UploadProtocolError();
  return parsed;
}

function parseSession(value: unknown): UploadSession {
  if (!isRecord(value)) throw new MalformedUploadResponse();
  const { id, projectId, fileName, totalSize, committedOffset, state } = value;
  if (
    typeof id !== "string" || !id || typeof projectId !== "string" || !projectId ||
    typeof fileName !== "string" || !fileName || typeof totalSize !== "string" ||
    typeof committedOffset !== "string" || !["active", "finalizing", "complete", "failed"].includes(String(state))
  ) throw new MalformedUploadResponse();
  const total = parseDecimal(totalSize);
  parseDecimal(committedOffset, total);
  return { id, projectId, fileName, totalSize, committedOffset, state: state as UploadState };
}

function storedFromSession(session: UploadSession): StoredUpload {
  return { uploadId: session.id, projectId: session.projectId, fileName: session.fileName,
    totalSize: session.totalSize, committedOffset: session.committedOffset };
}

function isAbort(reason: unknown, signal?: AbortSignal) {
  return signal?.aborted || (reason instanceof DOMException && reason.name === "AbortError") ||
    (reason instanceof Error && reason.name === "AbortError");
}

async function request(url: string, init: RequestInit, current?: UploadSession): Promise<Response> {
  try { return await fetch(url, init); }
  catch (reason) {
    if (isAbort(reason, init.signal ?? undefined)) throw reason;
    if (current) throw new UploadPausedError(current);
    throw reason;
  }
}

async function safeJson(response: Response): Promise<unknown> {
  try { return await response.json(); }
  catch { throw new MalformedUploadResponse(); }
}

function isRetryableHttpStatus(status: number) {
  return status === 408 || status === 425 || status === 429 || status >= 500;
}

async function safeHttpMessage(response: Response) {
  try {
    const body: unknown = await response.json();
    if (isRecord(body) && isRecord(body.error) && typeof body.error.message === "string" && body.error.message.trim()) {
      return body.error.message;
    }
  } catch {
    // Keep the stable fallback for proxies or malformed error envelopes.
  }
  return SAFE_FALLBACK;
}

async function throwClassifiedHttpError(response: Response, current?: UploadSession): Promise<never> {
  if (current && isRetryableHttpStatus(response.status)) {
    throw new UploadRetryableError(current, "The upload service is temporarily unavailable. Please retry.");
  }
  throw new UploadProtocolError(await safeHttpMessage(response));
}

async function jsonSession(response: Response, current?: UploadSession, retryable = false): Promise<UploadSession> {
  if (!response.ok) {
    return throwClassifiedHttpError(response, retryable ? current : undefined);
  }
  let parsed: UploadSession;
  try {
    parsed = parseSession(await safeJson(response));
  } catch (reason) {
    if (reason instanceof MalformedUploadResponse) {
      throw new UploadProtocolError();
    }
    throw reason;
  }
  if (current && parsed.id !== current.id) throw new UploadProtocolError();
  return parsed;
}

function validateIdentity(session: UploadSession, projectId: string, file: File) {
  if (session.projectId !== projectId || session.fileName !== file.name || session.totalSize !== String(file.size)) {
    throw new UploadProtocolError("The selected file does not match this upload session.");
  }
}

async function expectedConflictOffset(response: Response, maximum: number): Promise<number> {
  const header = response.headers.get("Upload-Offset");
  if (header !== null) return parseDecimal(header, maximum);
  let body: unknown;
  try { body = await safeJson(response); }
  catch { throw new UploadProtocolError(); }
  if (!isRecord(body) || !isRecord(body.error) || !isRecord(body.error.details)) throw new UploadProtocolError();
  return parseDecimal(body.error.details.expectedOffset, maximum);
}

export async function uploadFile({ projectId, file, session: suppliedSession, signal, onProgress,
  chunkSize = CHUNK_SIZE }: UploadOptions): Promise<UploadSession> {
  if (!Number.isSafeInteger(file.size) || file.size < 0 || !Number.isSafeInteger(chunkSize) || chunkSize <= 0) {
    throw new UploadProtocolError();
  }
  let current = suppliedSession;
  if (!current) {
    let createdResponse: Response;
    try {
      createdResponse = await request(`/api/v1/projects/${encodeURIComponent(projectId)}/uploads`, {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ fileName: file.name, totalSize: String(file.size) }), signal,
      });
    } catch (reason) {
      if (isAbort(reason, signal)) throw reason;
      throw new UploadProtocolError("The upload could not be started.");
    }
    current = await jsonSession(createdResponse);
    validateIdentity(current, projectId, file);
    saveStoredUpload(storedFromSession(current));
  } else validateIdentity(current, projectId, file);

  const statusResponse = await request(`/api/v1/uploads/${encodeURIComponent(current.id)}`,
    { method: "GET", signal }, current);
  current = await jsonSession(statusResponse, current, true);
  validateIdentity(current, projectId, file);
  saveStoredUpload(storedFromSession(current));
  const statusTotal = parseDecimal(current.totalSize);
  const statusOffset = parseDecimal(current.committedOffset, statusTotal);
  if (current.state === "complete") {
    if (statusOffset !== statusTotal) throw new UploadProtocolError();
    clearStoredUpload();
    return current;
  }
  if (current.state === "finalizing") {
    if (statusOffset !== statusTotal) throw new UploadProtocolError();
    throw new UploadRetryableError(current, "Finalization is still in progress. Please retry shortly.");
  }
  if (current.state === "failed") throw new UploadProtocolError("This upload session has failed and cannot be resumed.");

  const total = parseDecimal(current.totalSize);
  let offset = parseDecimal(current.committedOffset, total);
  onProgress?.({ committed: offset, total, session: current });
  while (offset < total) {
    const end = Math.min(offset + chunkSize, total);
    const response = await request(`/api/v1/uploads/${encodeURIComponent(current.id)}/chunk`, {
      method: "PUT", headers: { "Content-Type": "application/octet-stream", "Upload-Offset": String(offset) },
      body: file.slice(offset, end), signal,
    }, current);
    let authoritative: number;
    if (response.ok) {
      const returnedOffset = response.headers.get("Upload-Offset");
      if (returnedOffset === null) throw new UploadProtocolError();
      authoritative = parseDecimal(returnedOffset, total);
    } else if (response.status === 409) authoritative = await expectedConflictOffset(response, total);
    else return throwClassifiedHttpError(response, current);
    if (authoritative <= offset) throw new UploadProtocolError();
    offset = authoritative;
    current = { ...current, committedOffset: String(offset) };
    saveStoredUpload(storedFromSession(current));
    onProgress?.({ committed: offset, total, session: current });
  }

  const completeResponse = await request(`/api/v1/uploads/${encodeURIComponent(current.id)}/complete`,
    { method: "POST", signal }, current);
  if (completeResponse.status === 409) {
    const recheckResponse = await request(`/api/v1/uploads/${encodeURIComponent(current.id)}`,
      { method: "GET", signal }, current);
    const rechecked = await jsonSession(recheckResponse, current, true);
    validateIdentity(rechecked, projectId, file);
    saveStoredUpload(storedFromSession(rechecked));
    const recheckedOffset = parseDecimal(rechecked.committedOffset, total);
    if (rechecked.state === "complete") {
      if (recheckedOffset !== total) throw new UploadProtocolError();
      clearStoredUpload();
      return rechecked;
    }
    if (rechecked.state === "finalizing") {
      if (recheckedOffset !== total) throw new UploadProtocolError();
      throw new UploadRetryableError(rechecked, "Finalization is still in progress. Please retry shortly.");
    }
    if (rechecked.state === "failed") throw new UploadProtocolError("This upload session has failed and cannot be resumed.");
    throw new UploadProtocolError("The upload could not be completed safely.");
  }
  const complete = await jsonSession(completeResponse, current, true);
  validateIdentity(complete, projectId, file);
  if (parseDecimal(complete.committedOffset, total) !== total) throw new UploadProtocolError();
  if (complete.state === "failed") throw new UploadProtocolError("This upload session has failed and cannot be resumed.");
  if (complete.state !== "complete") throw new UploadRetryableError(complete, "Finalization is still in progress. Please retry shortly.");
  clearStoredUpload();
  return complete;
}

export function saveStoredUpload(metadata: StoredUpload) {
  if (!metadata.uploadId || !metadata.projectId || !metadata.fileName) throw new UploadProtocolError();
  const total = parseDecimal(metadata.totalSize);
  parseDecimal(metadata.committedOffset, total);
  const exact: StoredUpload = {
    uploadId: metadata.uploadId,
    projectId: metadata.projectId,
    fileName: metadata.fileName,
    totalSize: metadata.totalSize,
    committedOffset: metadata.committedOffset,
  };
  localStorage.setItem(STORAGE_KEY, JSON.stringify(exact));
}
export function clearStoredUpload() { localStorage.removeItem(STORAGE_KEY); }
export function loadStoredUpload(): StoredUpload | null {
  let raw: string | null;
  try { raw = localStorage.getItem(STORAGE_KEY); }
  catch { return null; }
  if (!raw) return null;
  const discardInvalid = () => {
    try { localStorage.removeItem(STORAGE_KEY); }
    catch { /* Storage can be disabled between reads and cleanup. */ }
    return null;
  };
  try {
    const value: unknown = JSON.parse(raw);
    if (!isRecord(value) || Object.keys(value).sort().join(",") !==
      "committedOffset,fileName,projectId,totalSize,uploadId") return discardInvalid();
    if (typeof value.uploadId !== "string" || !value.uploadId ||
      typeof value.projectId !== "string" || !value.projectId || typeof value.fileName !== "string" ||
      !value.fileName || typeof value.totalSize !== "string" || typeof value.committedOffset !== "string") return discardInvalid();
    const total = parseDecimal(value.totalSize);
    parseDecimal(value.committedOffset, total);
    return value as unknown as StoredUpload;
  } catch { return discardInvalid(); }
}
export function matchesStoredUpload(metadata: StoredUpload, projectId: string, file: File) {
  return metadata.projectId === projectId && metadata.fileName === file.name &&
    metadata.totalSize === String(file.size) && Number.isSafeInteger(file.size);
}
