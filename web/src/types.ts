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

export interface ErrorEnvelope {
  error: {
    code: string;
    message: string;
    requestId: string;
    details?: unknown;
  };
}
