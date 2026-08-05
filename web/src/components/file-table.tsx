import { FolderOpen, RefreshCw, Search } from "lucide-react";
import { useMemo, useState } from "react";

import { formatBinarySize, formatModifiedTime } from "../file-format";
import type { FileEntry } from "../types";

type LoadState = "loading" | "ready" | "error";
type FileSort = "name" | "newest" | "largest";

interface FileTableProps {
  projectId: string;
  state: LoadState;
  files: FileEntry[];
  error?: string;
  onRetry: () => void;
}

export function FileTable({ projectId, state, files, error, onRetry }: FileTableProps) {
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<FileSort>("name");
  const visibleFiles = useMemo(() => {
    const normalizedQuery = query.trim().toLocaleLowerCase("ko");
    return files
      .filter((file) => !normalizedQuery || file.name.toLocaleLowerCase("ko").includes(normalizedQuery))
      .sort((left, right) => {
        if (sort === "largest") return Number(right.size) - Number(left.size) || left.name.localeCompare(right.name, "ko");
        if (sort === "newest") return Date.parse(right.modifiedAt) - Date.parse(left.modifiedAt) || left.name.localeCompare(right.name, "ko");
        return left.name.localeCompare(right.name, "ko", { numeric: true, sensitivity: "base" });
      });
  }, [files, query, sort]);

  if (state === "loading") return <p className="inline-status" role="status">파일 불러오는 중…</p>;
  if (state === "error") {
    return (
      <div className="files-error">
        <p className="inline-error" role="alert">{error || "파일을 불러오지 못했습니다. 다시 시도해주세요."}</p>
        <button aria-label="파일 다시 불러오기" className="button button--secondary" onClick={onRetry} type="button">
          <RefreshCw aria-hidden="true" size={16} />다시 시도
        </button>
      </div>
    );
  }
  if (files.length === 0) {
    return (
      <div className="files-empty">
        <FolderOpen aria-hidden="true" size={22} strokeWidth={1.5} />
        <div><h2>아직 파일이 없습니다.</h2><p>위의 파일 올리기 버튼으로 첫 파일을 추가하세요.</p></div>
      </div>
    );
  }
  return (
    <div className="file-browser">
      <div className="file-browser__toolbar">
        <label className="file-browser__search">
          <Search aria-hidden="true" size={15} />
          <span className="sr-only">파일 검색</span>
          <input onChange={(event) => setQuery(event.target.value)} placeholder="파일 이름 검색" type="search" value={query} />
        </label>
        <label className="file-browser__sort">
          <span className="sr-only">파일 정렬</span>
          <select onChange={(event) => setSort(event.target.value as FileSort)} value={sort}>
            <option value="name">이름순</option>
            <option value="newest">최근 수정순</option>
            <option value="largest">큰 파일순</option>
          </select>
        </label>
        <span className="file-browser__count">{visibleFiles.length} / {files.length}</span>
      </div>

      {visibleFiles.length === 0 ? (
        <div className="files-empty files-empty--search"><div><h2>일치하는 파일이 없습니다.</h2><p>다른 검색어를 입력해보세요.</p></div></div>
      ) : (
        <div className="file-table-wrap">
          <table aria-label="프로젝트 파일" className="file-table">
            <thead><tr><th scope="col">이름</th><th scope="col">크기</th><th scope="col">수정일</th></tr></thead>
            <tbody>
              {visibleFiles.map((file) => {
                const formattedTime = formatModifiedTime(file.modifiedAt);
                const validTime = formattedTime !== file.modifiedAt;
                return (
                  <tr key={file.name}>
                    <th data-label="이름" scope="row">
                      <a href={`/api/v1/projects/${encodeURIComponent(projectId)}/files/${encodeURIComponent(file.name)}`}>
                        {file.name}
                      </a>
                    </th>
                    <td data-label="크기">{formatBinarySize(file.size)}</td>
                    <td data-label="수정일">{validTime ? <time dateTime={file.modifiedAt}>{formattedTime}</time> : file.modifiedAt}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
