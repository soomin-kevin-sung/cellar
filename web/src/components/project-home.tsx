import { ArrowRight, Files, HardDrive, Upload } from "lucide-react";
import type { RefObject } from "react";

import { formatBinarySize, formatModifiedTime } from "../file-format";
import type { FileEntry, Project } from "../types";

type LoadState = "loading" | "ready" | "error";

function totalSize(files: FileEntry[]) {
  let total = 0n;
  for (const file of files) {
    if (!/^(0|[1-9]\d*)$/.test(file.size)) return "확인 불가";
    total += BigInt(file.size);
  }
  return total <= BigInt(Number.MAX_SAFE_INTEGER) ? formatBinarySize(total.toString()) : `${total} B`;
}

export function ProjectHome({
  project,
  files,
  state,
  onBrowseFiles,
  onUpload,
  focusRef,
}: {
  project: Project;
  files: FileEntry[];
  state: LoadState;
  onBrowseFiles: () => void;
  onUpload: () => void;
  focusRef?: RefObject<HTMLElement | null>;
}) {
  const recentFiles = [...files]
    .sort((left, right) => Date.parse(right.modifiedAt) - Date.parse(left.modifiedAt))
    .slice(0, 4);

  return (
    <section aria-labelledby="project-home-title" className="project-home" ref={focusRef} tabIndex={-1}>
      <header className="project-home__header">
        <div>
          <p className="eyebrow">PROJECT / HOME</p>
          <h1 id="project-home-title">{project.name}</h1>
          <p>{state === "loading" ? "프로젝트 확인 중…" : state === "error" ? "파일 정보를 불러오지 못했습니다." : "이 프로젝트의 파일과 작업을 한곳에서 관리합니다."}</p>
        </div>
        <button className="button button--primary" onClick={onUpload} type="button">
          <Upload aria-hidden="true" size={17} />파일 올리기
        </button>
      </header>

      <div className="project-home__body">
        <button aria-label="파일 탐색" className="project-home__summary-card" onClick={onBrowseFiles} type="button">
          <span className="project-home__summary-icon"><Files aria-hidden="true" size={22} strokeWidth={1.6} /></span>
          <span>
            <small>FILES</small>
            <strong>{state === "ready" ? files.length : "—"}</strong>
            <em>{state === "ready" ? totalSize(files) : "불러오는 중"}</em>
          </span>
          <span className="project-home__summary-action">파일 탐색 <ArrowRight aria-hidden="true" size={16} /></span>
        </button>

        <section aria-labelledby="recent-files-title" className="project-home__recent">
          <header>
            <div>
              <HardDrive aria-hidden="true" size={17} />
              <h2 id="recent-files-title">최근 파일</h2>
            </div>
            <button className="button button--quiet" onClick={onBrowseFiles} type="button">전체 보기 <ArrowRight aria-hidden="true" size={15} /></button>
          </header>

          {state === "loading" ? <p className="project-home__status" role="status">파일을 불러오는 중…</p> : null}
          {state === "error" ? <p className="project-home__status">파일 정보를 불러오지 못했습니다.</p> : null}
          {state === "ready" && recentFiles.length === 0 ? (
            <button className="project-home__empty" onClick={onUpload} type="button">
              <strong>아직 파일이 없습니다.</strong>
              <span>첫 파일을 올려 프로젝트를 시작하세요.</span>
            </button>
          ) : null}
          {state === "ready" && recentFiles.length > 0 ? (
            <div className="project-home__recent-list">
              {recentFiles.map((file) => (
                <button key={file.name} onClick={onBrowseFiles} type="button">
                  <span>{file.name}</span>
                  <small>{formatBinarySize(file.size)}</small>
                  <time dateTime={file.modifiedAt}>{formatModifiedTime(file.modifiedAt)}</time>
                </button>
              ))}
            </div>
          ) : null}
        </section>
      </div>
    </section>
  );
}
