import { Folder, LogOut, Plus, Upload, Users } from "lucide-react";

import type { ReactNode } from "react";
import type { Project } from "../types";
import type { CurrentUser } from "../types";

interface AppShellProps {
  activeProjectId: string | null;
  activeSection: "project-home" | "projects" | "upload" | "admin" | null;
  projects: Project[];
  selectedProjectId: string | null;
  onSelectProject: (projectId: string) => void;
  onCreateProject: () => void;
  onOpenHome: () => void;
  onOpenProjectHome: () => void;
  onOpenUploads: () => void;
  showCreateAction: boolean;
  currentUser?: CurrentUser;
  onOpenAdmin?: () => void;
  onLogout?: () => void;
  children: ReactNode;
}

export function AppShell({
  activeProjectId,
  activeSection,
  projects,
  selectedProjectId,
  onSelectProject,
  onCreateProject,
  onOpenHome,
  onOpenProjectHome,
  onOpenUploads,
  showCreateAction,
  currentUser,
  onOpenAdmin,
  onLogout,
  children,
}: AppShellProps) {
  return (
    <div className={`app-shell${showCreateAction ? " app-shell--create-visible" : ""}`}>
      <aside className="sidebar">
        <button className="sidebar__heading" onClick={onOpenHome} type="button">
          <Folder aria-hidden="true" size={18} strokeWidth={1.8} />
          <span>프로젝트</span>
        </button>

        <nav className="project-nav" aria-label="프로젝트 목록">
          <div className="project-nav__list">
            {projects.map((project) => (
              <button
                aria-current={activeProjectId === project.id ? "page" : undefined}
                className="project-nav__item"
                key={project.id}
                onClick={() => onSelectProject(project.id)}
                type="button"
              >
                <span aria-hidden="true" className="project-nav__dot" />
                <span>{project.name}</span>
              </button>
            ))}
          </div>
        </nav>

        <nav className="secondary-nav" aria-label="작업 메뉴">
          <button aria-current={activeSection === "upload" ? "page" : undefined} onClick={onOpenUploads} type="button"><Upload aria-hidden="true" size={17} />업로드</button>
          {currentUser?.role === "admin" ? <button aria-current={activeSection === "admin" ? "page" : undefined} onClick={onOpenAdmin} type="button"><Users aria-hidden="true" size={17} />사용자</button> : null}
          {currentUser ? (
            <div className="account-nav">
              <div><strong>{currentUser.username}</strong><span>{currentUser.role === "admin" ? "관리자" : "사용자"}</span></div>
              <button aria-label="로그아웃" onClick={onLogout} type="button"><LogOut aria-hidden="true" size={17} /></button>
            </div>
          ) : null}
        </nav>
      </aside>

      <div className="workspace">
        <header className="mobile-bar">
          <label className="sr-only" htmlFor="mobile-project-select">프로젝트 선택</label>
          <div className="mobile-select-wrap">
            <Folder aria-hidden="true" size={17} />
            <select
              aria-label="프로젝트 선택"
              id="mobile-project-select"
              onChange={(event) => onSelectProject(event.target.value)}
              value={selectedProjectId ?? ""}
            >
              {projects.length === 0 ? <option value="">프로젝트 없음</option> : null}
              {projects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}
            </select>
          </div>
          <button aria-label="업로드" className="mobile-upload" onClick={onOpenUploads} type="button">
            <Upload aria-hidden="true" size={17} />
          </button>
          <button aria-label="프로젝트 홈" className="mobile-upload" onClick={onOpenProjectHome} type="button">
            <Folder aria-hidden="true" size={17} />
          </button>
        </header>

        {showCreateAction ? (
          <button className="button button--primary workspace__create" type="button" onClick={onCreateProject}>
            <Plus aria-hidden="true" size={17} strokeWidth={2.2} />
            프로젝트 만들기
          </button>
        ) : null}

        <main className="workspace__main">{children}</main>
      </div>
    </div>
  );
}
