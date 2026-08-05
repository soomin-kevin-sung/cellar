import { RefreshCw, Upload } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ChangeEvent } from "react";

import { api } from "./api";
import { AppShell } from "./components/app-shell";
import { AdminPanel } from "./components/admin-panel";
import { EmptyState } from "./components/empty-state";
import { FeaturePortal, type FeaturePortalOrigin, type FeaturePortalPhase } from "./components/feature-portal";
import { FileTable } from "./components/file-table";
import { ProjectCreateDialog } from "./components/project-create-dialog";
import { ProjectHome } from "./components/project-home";
import { UploadDock } from "./components/upload-dock";
import { UploadPanel, type UploadTask } from "./components/upload-panel";
import { formatFileSummary } from "./file-format";
import type { CurrentUser, FileEntry, Project } from "./types";
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

function prefersReducedMotion() {
  return typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export default function App({
  client = api,
  uploader = uploadFile,
  currentUser,
  onLogout,
}: {
  client?: ApiClient;
  uploader?: typeof uploadFile;
  currentUser?: CurrentUser;
  onLogout?: () => Promise<void> | void;
}) {
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
  const [section, setSection] = useState<"project-home" | "projects" | "upload" | "admin">("projects");
  const [uploadTask, setUploadTask] = useState<UploadTask | null>(null);
  const [portalPhase, setPortalPhase] = useState<FeaturePortalPhase>("idle");
  const [portalOrigin, setPortalOrigin] = useState<FeaturePortalOrigin>({ column: 2, row: 2 });
  const fileRequestRef = useRef(0);
  const portalPhaseRef = useRef<FeaturePortalPhase>("idle");
  const activeFeatureKeyRef = useRef<string | null>(null);
  const portalTimersRef = useRef<number[]>([]);
  const workspaceFocusRef = useRef<HTMLElement>(null);
  const uploadFocusRef = useRef<HTMLElement>(null);
  const projectUploadInputRef = useRef<HTMLInputElement>(null);
  const uploadControllerRef = useRef<AbortController | null>(null);
  const selectedProjectIdRef = useRef<string | null>(null);
  selectedProjectIdRef.current = selectedProjectId;

  const clearPortalTimers = () => {
    portalTimersRef.current.forEach((timer) => window.clearTimeout(timer));
    portalTimersRef.current = [];
  };

  const randomPortalOrigin = (): FeaturePortalOrigin => ({
    column: Math.floor(Math.random() * 5),
    row: Math.floor(Math.random() * 5),
  });

  const changePortalPhase = (phase: FeaturePortalPhase) => {
    portalPhaseRef.current = phase;
    setPortalPhase(phase);
  };

  const changeActiveFeature = (featureKey: string | null) => {
    activeFeatureKeyRef.current = featureKey;
  };

  const openFeature = (featureKey: string, activate: () => void) => {
    const currentPhase = portalPhaseRef.current;
    if (currentPhase !== "idle" && activeFeatureKeyRef.current === featureKey) return;

    clearPortalTimers();
    setPortalOrigin(randomPortalOrigin());
    const activateFeature = () => {
      activate();
      changeActiveFeature(featureKey);
    };
    const revealFeature = () => {
      activateFeature();
      changePortalPhase("opening");
      portalTimersRef.current.push(window.setTimeout(() => changePortalPhase("open"), 920));
    };

    if (prefersReducedMotion()) {
      activateFeature();
      changePortalPhase("open");
      return;
    }

    if (currentPhase === "idle") {
      revealFeature();
      return;
    }

    changePortalPhase("closing");
    portalTimersRef.current.push(window.setTimeout(() => {
      revealFeature();
    }, 580));
  };

  const returnToCube = () => {
    clearPortalTimers();
    setPortalOrigin(randomPortalOrigin());
    if (prefersReducedMotion() || portalPhaseRef.current === "idle") {
      changePortalPhase("idle");
      changeActiveFeature(null);
      return;
    }
    changePortalPhase("closing");
    portalTimersRef.current.push(window.setTimeout(() => {
      changePortalPhase("idle");
      changeActiveFeature(null);
    }, 580));
  };

  useEffect(() => () => {
    portalTimersRef.current.forEach((timer) => window.clearTimeout(timer));
    uploadControllerRef.current?.abort();
  }, []);

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
      setProjectsError(safeMessage(reason, "프로젝트를 불러오지 못했습니다. 다시 시도해주세요."));
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
      setFilesError(safeMessage(reason, "파일을 불러오지 못했습니다. 다시 시도해주세요."));
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
    openFeature(`project-home:${created.id}`, () => {
      setProjects((current) => current.some((project) => project.id === created.id) ? current : [...current, created]);
      setFiles([]);
      setFilesState("loading");
      setFilesError("");
      setSelectedProjectId(created.id);
      setProjectsState("ready");
      setSection("project-home");
    });
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

  const runUpload = async (file: File, project: Project) => {
    if (uploadControllerRef.current) return;
    const controller = new AbortController();
    uploadControllerRef.current = controller;
    setUploadTask({
      projectId: project.id,
      projectName: project.name,
      file,
      state: "uploading",
      progress: 0,
      result: null,
      message: "",
    });
    try {
      const result = await uploader({
        projectId: project.id,
        file,
        onProgress: (uploadedBytes, totalBytes) => {
          const progress = totalBytes > 0 ? Math.round((uploadedBytes / totalBytes) * 100) : 0;
          setUploadTask((current) => current?.file === file ? { ...current, progress } : current);
        },
        signal: controller.signal,
      });
      setUploadTask((current) => current?.file === file ? {
        ...current,
        state: "complete",
        progress: 100,
        result,
      } : current);
      if (selectedProjectIdRef.current === project.id) retryFiles();
    } catch (reason) {
      if (controller.signal.aborted) return;
      setUploadTask((current) => current?.file === file ? {
        ...current,
        state: "error",
        message: safeMessage(reason, "파일을 업로드하지 못했습니다. 다시 시도해주세요."),
      } : current);
    } finally {
      if (uploadControllerRef.current === controller) uploadControllerRef.current = null;
    }
  };

  const retryUpload = () => {
    if (!uploadTask || uploadTask.state === "uploading") return;
    void runUpload(uploadTask.file, { id: uploadTask.projectId, name: uploadTask.projectName, createdAt: "" });
  };

  const resetUpload = () => {
    if (uploadTask?.state === "uploading") return;
    setUploadTask(null);
  };

  const projectUploadChanged = (event: ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (file && selectedProject) void runUpload(file, selectedProject);
  };

  const openUploadTask = () => {
    if (!uploadTask) return;
    openFeature(`upload:${uploadTask.projectId}`, () => {
      setSection("upload");
      selectProject(uploadTask.projectId);
    });
  };

  useEffect(() => {
    if (section !== "upload" || portalPhase !== "open") return;
    const panel = uploadFocusRef.current;
    if (!panel) return;
    panel.focus({ preventScroll: true });
    panel.scrollIntoView?.({ behavior: prefersReducedMotion() ? "auto" : "smooth", block: "center" });
  }, [portalPhase, section]);

  const effectivePortalPhase = projectsState === "ready" ? portalPhase : "open";

  return (
    <>
      <div aria-hidden={dialogOpen ? "true" : undefined}>
        <AppShell
          activeProjectId={portalPhase === "idle" || (section !== "project-home" && section !== "projects") ? null : selectedProjectId}
          activeSection={portalPhase === "idle" ? null : section}
          onCreateProject={() => setDialogOpen(true)}
          currentUser={currentUser}
          onLogout={() => { void onLogout?.(); }}
          onOpenAdmin={() => openFeature("admin", () => setSection("admin"))}
          onOpenHome={returnToCube}
          onOpenProjectHome={() => {
            if (!selectedProjectId) return;
            openFeature(`project-home:${selectedProjectId}`, () => setSection("project-home"));
          }}
          onOpenUploads={() => {
            if (!selectedProjectId) return;
            openFeature(`upload:${selectedProjectId}`, () => setSection("upload"));
          }}
          onSelectProject={(projectId) => openFeature(`project-home:${projectId}`, () => {
            setSection("project-home");
            selectProject(projectId);
          })}
          projects={projects}
          selectedProjectId={selectedProjectId}
          showCreateAction={portalPhase === "idle" && projectsState === "ready"}
        >
        <FeaturePortal
          origin={portalOrigin}
          phase={effectivePortalPhase}
        >
        {selectedProject ? (
          <input
            aria-label="현재 프로젝트에 파일 업로드"
            className="sr-only"
            disabled={uploadTask?.state === "uploading"}
            onChange={projectUploadChanged}
            ref={projectUploadInputRef}
            type="file"
          />
        ) : null}
        {section === "admin" && currentUser?.role === "admin" ? <AdminPanel currentUser={currentUser} /> : null}

        {section === "project-home" && projectsState === "ready" && selectedProject ? (
          <ProjectHome
            files={files}
            focusRef={workspaceFocusRef}
            onBrowseFiles={() => openFeature(`project:${selectedProject.id}`, () => {
              setSection("projects");
            })}
            onUpload={() => projectUploadInputRef.current?.click()}
            project={selectedProject}
            state={filesState}
          />
        ) : null}

        {section === "projects" ? <>
        {projectsState === "loading" ? (
          <section aria-label="프로젝트 불러오는 중" className="loading-state" role="status">
            <span className="skeleton skeleton--title" />
            <span className="skeleton skeleton--line" />
            <span className="skeleton skeleton--panel" />
          </section>
        ) : null}

        {projectsState === "error" ? (
          <section className="message-state message-state--error">
            <div aria-live="assertive" role="alert">
              <h1>프로젝트를 불러오지 못했습니다</h1>
              <p>{projectsError}</p>
            </div>
            <button className="button button--secondary" onClick={() => {
              setProjectsState("loading");
              setProjectsError("");
              setReloadKey((key) => key + 1);
            }} type="button">
              <RefreshCw aria-hidden="true" size={17} />다시 시도
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
              <div>
                <p className="eyebrow">프로젝트 / 파일</p>
                <h1 id="project-title">{selectedProject.name}</h1>
                <p className="project-workspace__summary">
                  {filesState === "loading" ? "파일 정보 불러오는 중…" : filesState === "error" ? "파일 정보를 불러오지 못했습니다" : formatFileSummary(files)}
                </p>
              </div>
              <button
                className="button button--primary project-workspace__upload"
                disabled={uploadTask?.state === "uploading"}
                onClick={() => projectUploadInputRef.current?.click()}
                type="button"
              >
                <Upload aria-hidden="true" size={16} />파일 올리기
              </button>
            </header>

            <div className="files-panel" aria-live="polite">
              <FileTable error={filesError} files={files} onRetry={retryFiles} projectId={selectedProject.id} state={filesState} />
            </div>

          </section>
        ) : null}
        </> : null}

        {section === "upload" && selectedProject ? (
          <section className="upload-workspace" aria-labelledby="feature-upload-title">
            <header className="project-workspace__header">
              <p className="eyebrow">프로젝트</p>
              <h1 id="feature-upload-title">{selectedProject.name}</h1>
              <p className="project-workspace__summary">이 PC로 파일을 전송합니다.</p>
            </header>
            <UploadPanel
              onReset={resetUpload}
              onRetry={retryUpload}
              onUpload={(file) => { void runUpload(file, selectedProject); }}
              projectId={selectedProject.id}
              projectName={selectedProject.name}
              ref={uploadFocusRef}
              task={uploadTask}
            />
          </section>
        ) : null}
        </FeaturePortal>
        {uploadTask && !(section === "upload" && portalPhase === "open") ? (
          <UploadDock
            onOpen={openUploadTask}
            onReset={resetUpload}
            onRetry={retryUpload}
            task={uploadTask}
          />
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
