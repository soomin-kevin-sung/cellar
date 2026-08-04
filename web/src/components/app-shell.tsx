import { Folder, Plus, Settings, Upload } from "lucide-react";

import type { ReactNode } from "react";
import type { Project } from "../types";

interface AppShellProps {
  projects: Project[];
  selectedProjectId: string | null;
  onSelectProject: (projectId: string) => void;
  onCreateProject: () => void;
  onOpenUploads: () => void;
  showCreateAction: boolean;
  children: ReactNode;
}

export function AppShell({
  projects,
  selectedProjectId,
  onSelectProject,
  onCreateProject,
  onOpenUploads,
  showCreateAction,
  children,
}: AppShellProps) {
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="sidebar__heading">
          <Folder aria-hidden="true" size={18} strokeWidth={1.8} />
          <span>Projects</span>
        </div>

        <nav className="project-nav" aria-label="Projects navigation">
          <div className="project-nav__list">
            {projects.map((project) => (
              <button
                aria-current={selectedProjectId === project.id ? "page" : undefined}
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

        <nav className="secondary-nav" aria-label="Workspace navigation">
          <button onClick={onOpenUploads} type="button"><Upload aria-hidden="true" size={17} />Uploads</button>
          <button type="button"><Settings aria-hidden="true" size={17} />Settings</button>
        </nav>
      </aside>

      <div className="workspace">
        <header className="mobile-bar">
          <label className="sr-only" htmlFor="mobile-project-select">Select project</label>
          <div className="mobile-select-wrap">
            <Folder aria-hidden="true" size={17} />
            <select
              aria-label="Select project"
              id="mobile-project-select"
              onChange={(event) => onSelectProject(event.target.value)}
              value={selectedProjectId ?? ""}
            >
              {projects.length === 0 ? <option value="">No projects</option> : null}
              {projects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}
            </select>
          </div>
          <button aria-label="Uploads" className="mobile-upload" onClick={onOpenUploads} type="button">
            <Upload aria-hidden="true" size={17} />
          </button>
        </header>

        {showCreateAction ? (
          <button className="button button--primary workspace__create" type="button" onClick={onCreateProject}>
            <Plus aria-hidden="true" size={17} strokeWidth={2.2} />
            Create project
          </button>
        ) : null}

        <main className="workspace__main">{children}</main>
      </div>
    </div>
  );
}
