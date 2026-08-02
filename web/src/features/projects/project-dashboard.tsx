import { useMemo, useState } from "react";
import {
  ArrowUpRight,
  CheckCircle2,
  ChevronRight,
  FileArchive,
  FileImage,
  FileText,
  Film,
  FolderOpen,
  Grid2X2,
  List,
  MoreHorizontal,
  Plus,
  Search,
  Eye,
  Upload,
} from "lucide-react";

type Project = {
  id: string;
  name: string;
  description: string;
  files: string;
  size: string;
  updated: string;
  tone: "wine" | "forest" | "ink";
  visual: "bottles" | "sheets" | "frames";
};

const projects: Project[] = [
  {
    id: "cellar",
    name: "Cellar",
    description: "Product notes, releases and the files behind this workspace.",
    files: "148 files",
    size: "2.4 GB",
    updated: "오늘",
    tone: "wine",
    visual: "bottles",
  },
  {
    id: "archive-2025",
    name: "Archive 2025",
    description: "A quiet record of completed work and personal documents.",
    files: "1,204 files",
    size: "38.2 GB",
    updated: "어제",
    tone: "forest",
    visual: "sheets",
  },
  {
    id: "studio-references",
    name: "Studio References",
    description: "Textures, references and visual fragments worth keeping.",
    files: "386 files",
    size: "16.8 GB",
    updated: "7월 29일",
    tone: "ink",
    visual: "frames",
  },
];

const recentFiles = [
  { name: "cellar-system-map.pdf", project: "Cellar", size: "4.2 MB", time: "12분 전", kind: "pdf" },
  { name: "release-notes-v0.2.md", project: "Cellar", size: "18 KB", time: "1시간 전", kind: "text" },
  { name: "summer-reference-04.jpg", project: "Studio References", size: "8.7 MB", time: "어제", kind: "image" },
  { name: "workspace-walkthrough.mp4", project: "Archive 2025", size: "184 MB", time: "8월 1일", kind: "video" },
];

function ProjectArtwork({ visual }: Pick<Project, "visual">) {
  if (visual === "bottles") {
    return <div className="artwork artwork-bottles" aria-hidden="true"><i /><i /><i /><span>CEL·LAR</span></div>;
  }
  if (visual === "sheets") {
    return <div className="artwork artwork-sheets" aria-hidden="true"><i /><i /><i /><span>25</span></div>;
  }
  return <div className="artwork artwork-frames" aria-hidden="true"><i /><i /><i /><span /></div>;
}

function ProjectCard({ project, index }: { project: Project; index: number }) {
  return (
    <article className={`project-card project-${project.tone}`} style={{ "--delay": `${index * 80}ms` } as React.CSSProperties}>
      <button className="project-open" type="button" aria-label={`${project.name} 열기`}>
        <ProjectArtwork visual={project.visual} />
        <span className="project-copy">
          <span className="project-title-row">
            <span><span className="project-kicker">Project · {project.updated}</span><h2>{project.name}</h2></span>
            <span className="project-arrow"><ArrowUpRight aria-hidden="true" size={18} /></span>
          </span>
          <span className="project-description">{project.description}</span>
          <span className="project-meta"><span>{project.files}</span><i /><span>{project.size}</span></span>
        </span>
      </button>
    </article>
  );
}

function FileGlyph({ kind }: { kind: string }) {
  const Icon = kind === "image" ? FileImage : kind === "video" ? Film : kind === "pdf" ? FileArchive : FileText;
  return <span className={`file-glyph file-${kind}`}><Icon aria-hidden="true" size={17} strokeWidth={1.7} /></span>;
}

export function ProjectDashboard() {
  const [query, setQuery] = useState("");
  const [view, setView] = useState<"grid" | "list">("grid");
  const filteredProjects = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    if (!normalized) return projects;
    return projects.filter((project) => `${project.name} ${project.description}`.toLocaleLowerCase().includes(normalized));
  }, [query]);

  return (
    <div className="dashboard">
      <section className="dashboard-intro" aria-labelledby="dashboard-title">
        <div>
          <p className="eyebrow"><span>Private archive</span><i /> 2026.08.03</p>
          <h1 id="dashboard-title">프로젝트 보관함</h1>
          <p>당신의 PC에 머무는 파일을, 어디서든 차분하게 정리하세요.</p>
        </div>
        <button className="primary-button" type="button"><Plus aria-hidden="true" size={17} /> 새 프로젝트</button>
      </section>

      <section className="archive-summary" aria-label="보관함 요약">
        <div><span>Active projects</span><strong>03</strong><small>모두 최신 상태</small></div>
        <div><span>Library</span><strong>1,738</strong><small>파일과 폴더</small></div>
        <div><span>Remote session</span><strong className="summary-preview"><Eye aria-hidden="true" size={22} /> Preview</strong><small>Access 연동 전</small></div>
        <div className="summary-note"><span className="summary-orbit"><i /><i /><i /></span><p>모든 원본은<br /><strong>이 PC에만</strong> 보관됩니다.</p></div>
      </section>

      <section className="projects-section" aria-label="프로젝트 목록">
        <div className="section-heading">
          <div><p className="section-number">01</p><h2>Projects</h2><span>{filteredProjects.length.toString().padStart(2, "0")}</span></div>
          <div className="project-tools">
            <label className="project-search"><Search aria-hidden="true" size={15} /><span className="sr-only">프로젝트 검색</span><input aria-label="프로젝트 검색" type="search" placeholder="프로젝트 찾기" value={query} onChange={(event) => setQuery(event.target.value)} /></label>
            <span className="view-switch" aria-label="보기 방식">
              <button className={view === "grid" ? "active" : ""} type="button" aria-label="그리드 보기" aria-pressed={view === "grid"} onClick={() => setView("grid")}><Grid2X2 aria-hidden="true" size={14} /></button>
              <button className={view === "list" ? "active" : ""} type="button" aria-label="목록 보기" aria-pressed={view === "list"} onClick={() => setView("list")}><List aria-hidden="true" size={15} /></button>
            </span>
          </div>
        </div>

        <div className={view === "grid" ? "project-grid" : "project-grid project-grid-list"}>
          {filteredProjects.map((project, index) => <ProjectCard key={project.id} project={project} index={index} />)}
          {filteredProjects.length === 0 ? <div className="empty-projects"><FolderOpen aria-hidden="true" size={24} /><p>일치하는 프로젝트가 없습니다.</p><button type="button" onClick={() => setQuery("")}>검색 지우기</button></div> : null}
        </div>
      </section>

      <section className="lower-grid">
        <div className="recent-panel">
          <div className="panel-heading"><div><p className="section-number">02</p><h2>최근 파일</h2></div><button type="button">모두 보기 <ChevronRight aria-hidden="true" size={15} /></button></div>
          <div className="file-table" role="table" aria-label="최근 파일">
            <div className="file-row file-header" role="row"><span role="columnheader">Name</span><span role="columnheader">Project</span><span role="columnheader">Size</span><span role="columnheader">Modified</span><span /></div>
            {recentFiles.map((file) => (
              <button className="file-row" role="row" type="button" key={file.name}>
                <span role="cell" className="file-name"><FileGlyph kind={file.kind} /><span><strong>{file.name}</strong><small>{file.project}</small></span></span>
                <span role="cell" className="file-project">{file.project}</span>
                <span role="cell">{file.size}</span>
                <span role="cell">{file.time}</span>
                <span role="cell"><MoreHorizontal aria-hidden="true" size={17} /></span>
              </button>
            ))}
          </div>
        </div>

        <aside className="transfer-panel" aria-labelledby="transfer-title">
          <div className="panel-heading"><div><p className="section-number">03</p><h2 id="transfer-title">전송</h2></div><button className="round-action" type="button" aria-label="파일 업로드"><Upload aria-hidden="true" size={16} /></button></div>
          <div className="transfer-illustration" aria-hidden="true"><span className="transfer-ring"><i /><i /><i /></span><strong>72<small>%</small></strong></div>
          <div className="transfer-copy"><span><strong>archive-pack.zip</strong><small>Archive 2025 · 486 MB</small></span><span>348 MB</span></div>
          <div className="transfer-track"><span /></div>
          <div className="transfer-footer"><span><CheckCircle2 aria-hidden="true" size={14} /> 데모 전송</span><button type="button">세부 정보</button></div>
        </aside>
      </section>
    </div>
  );
}
