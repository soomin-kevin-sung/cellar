import { RefreshCw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { api } from "./api";
import { AppShell } from "./components/app-shell";
import { EmptyState } from "./components/empty-state";
import { FileTable } from "./components/file-table";
import { ProjectCreateDialog } from "./components/project-create-dialog";
import { UploadPanel } from "./components/upload-panel";
import { formatFileSummary } from "./file-format";
import type { FileEntry, Project } from "./types";
import { uploadFile } from "./upload-client";

export interface ApiClient {
  listProjects(signal?: AbortSignal): Promise<Project[]>;
  createProject(name: string, signal?: AbortSignal): Promise<Project>;
  listFiles(projectId: string, signal?: AbortSignal): Promise<FileEntry[]>;
}

type LoadState = "loading" | "ready" | "error";

function safeMessage(reason: unknown, fallback: string) {
  return reason instanceof Error && reason.message.trim() ? reason.message : fallback;
}

export default function App({ client = api, uploader = uploadFile }: { client?: ApiClient; uploader?: typeof uploadFile }) {
  const [projects, setProjects] = useState<Project[]>([]);
  const [projectsState, setProjectsState] = useState<LoadState>("loading");
  const [projectsError, setProjectsError] = useState("");
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
  const [files, setFiles] = useState<FileEntry[]>([]);
  const [filesState, setFilesState] = useState<LoadState>("loading");
  const [filesError, setFilesError] = useState("");
  const [filesReloadKey, setFilesReloadKey] = useState(0);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const fileRequestRef = useRef(0);
  const workspaceFocusRef = useRef<HTMLElement>(null);
  const uploadFocusRef = useRef<HTMLElement>(null);

  useEffect(() => {
    const controller = new AbortController();

    client.listProjects(controller.signal).then((returnedProjects) => {
      setProjects(returnedProjects);
      setSelectedProjectId((current) =>
        current && returnedProjects.some((project) => project.id === current)
          ? current
          : (returnedProjects[0]?.id ?? null),
      );
      setProjectsState("ready");
    }).catch((reason: unknown) => {
      if (reason instanceof DOMException && reason.name === "AbortError") return;
      setProjectsError(safeMessage(reason, "Projects could not be loaded. Please try again."));
      setProjectsState("error");
    });

    return () => controller.abort();
  }, [client, reloadKey]);

  useEffect(() => {
    if (!selectedProjectId) return;

    const controller = new AbortController();
    const requestId = ++fileRequestRef.current;
    client.listFiles(selectedProjectId, controller.signal).then((returnedFiles) => {
      if (fileRequestRef.current !== requestId) return;
      setFiles(returnedFiles);
      setFilesState("ready");
    }).catch((reason: unknown) => {
      if (controller.signal.aborted || fileRequestRef.current !== requestId) return;
      setFilesError(safeMessage(reason, "Files could not be loaded. Please try again."));
      setFilesState("error");
    });

    return () => controller.abort();
  }, [client, filesReloadKey, selectedProjectId]);

  const selectedProject = useMemo(
    () => projects.find((project) => project.id === selectedProjectId) ?? null,
    [projects, selectedProjectId],
  );

  const createProject = async (name: string) => {
    const created = await client.createProject(name);
    setProjects((current) => current.some((project) => project.id === created.id) ? current : [...current, created]);
    setFiles([]);
    setFilesState("loading");
    setFilesError("");
    setSelectedProjectId(created.id);
    setProjectsState("ready");
    return created;
  };

  const selectProject = (projectId: string) => {
    if (projectId === selectedProjectId) return;
    setFiles([]);
    setFilesState("loading");
    setFilesError("");
    setSelectedProjectId(projectId);
  };

  const retryFiles = () => {
    setFiles([]);
    setFilesState("loading");
    setFilesError("");
    setFilesReloadKey((key) => key + 1);
  };

  return (
    <>
      <div aria-hidden={dialogOpen ? "true" : undefined}>
        <AppShell
          onCreateProject={() => setDialogOpen(true)}
          onOpenUploads={() => {
            const panel = uploadFocusRef.current;
            if (!panel) return;
            panel.focus({ preventScroll: true });
            const reducedMotion = typeof window.matchMedia === "function" &&
              window.matchMedia("(prefers-reduced-motion: reduce)").matches;
            panel.scrollIntoView?.({ behavior: reducedMotion ? "auto" : "smooth", block: "center" });
          }}
          onSelectProject={selectProject}
          projects={projects}
          selectedProjectId={selectedProjectId}
          showCreateAction={projectsState === "ready" && projects.length > 0}
        >
        {projectsState === "loading" ? (
          <section aria-label="Loading projects" className="loading-state" role="status">
            <span className="skeleton skeleton--title" />
            <span className="skeleton skeleton--line" />
            <span className="skeleton skeleton--panel" />
          </section>
        ) : null}

        {projectsState === "error" ? (
          <section className="message-state message-state--error">
            <div aria-live="assertive" role="alert">
              <h1>Projects unavailable</h1>
              <p>{projectsError}</p>
            </div>
            <button className="button button--secondary" onClick={() => {
              setProjectsState("loading");
              setProjectsError("");
              setReloadKey((key) => key + 1);
            }} type="button">
              <RefreshCw aria-hidden="true" size={17} />Try again
            </button>
          </section>
        ) : null}

        {projectsState === "ready" && projects.length === 0 ? (
          <EmptyState onCreateProject={() => setDialogOpen(true)} />
        ) : null}

        {projectsState === "ready" && selectedProject ? (
          <section
            aria-labelledby="project-title"
            className="project-workspace"
            ref={workspaceFocusRef}
            tabIndex={-1}
          >
            <header className="project-workspace__header">
              <p className="eyebrow">Project</p>
              <h1 id="project-title">{selectedProject.name}</h1>
              <p className="project-workspace__summary">
                {filesState === "loading" ? "Loading file summary…" : filesState === "error" ? "File summary unavailable" : formatFileSummary(files)}
              </p>
            </header>

            <div className="files-panel" aria-live="polite">
              <FileTable error={filesError} files={files} onRetry={retryFiles} projectId={selectedProject.id} state={filesState} />
            </div>

            <UploadPanel
              key={selectedProject.id}
              onComplete={retryFiles}
              projectId={selectedProject.id}
              projectName={selectedProject.name}
              ref={uploadFocusRef}
              upload={uploader}
            />
          </section>
        ) : null}
        </AppShell>
      </div>

      <ProjectCreateDialog
        fallbackFocusRef={workspaceFocusRef}
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreate={createProject}
      />
    </>
  );
}
