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

export type UserRole = "admin" | "member";

export interface CurrentUser {
  id: string;
  username: string;
  role: UserRole;
}

export interface ManagedUser extends CurrentUser {
  active: boolean;
  createdAt: number;
}

export interface ErrorEnvelope {
  error: {
    code: string;
    message: string;
    requestId: string;
    details?: unknown;
  };
}
