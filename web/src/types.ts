export interface Project {
  id: string;
  name: string;
  createdAt: string;
}

export interface FileEntry {
  name: string;
  size: string;
  modifiedAt: string;
}

export type UploadState = "active" | "finalizing" | "complete" | "failed";

export interface UploadSession {
  id: string;
  projectId: string;
  fileName: string;
  totalSize: string;
  committedOffset: string;
  state: UploadState;
}

export interface ErrorEnvelope {
  error: {
    code: string;
    message: string;
    requestId: string;
    details?: unknown;
  };
}
