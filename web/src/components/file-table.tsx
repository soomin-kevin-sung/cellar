import { RefreshCw } from "lucide-react";
import type { FileEntry } from "../types";
import { formatBinarySize, formatModifiedTime } from "../file-format";

type LoadState = "loading" | "ready" | "error";

interface FileTableProps {
  projectId: string;
  state: LoadState;
  files: FileEntry[];
  error?: string;
  onRetry: () => void;
}

export function FileTable({ projectId, state, files, error, onRetry }: FileTableProps) {
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
        <div><h2>아직 파일이 없습니다.</h2><p>아래에서 파일을 선택해 업로드하세요.</p></div>
      </div>
    );
  }
  return (
    <div className="file-table-wrap">
      <table aria-label="프로젝트 파일" className="file-table">
        <thead><tr><th scope="col">이름</th><th scope="col">크기</th><th scope="col">수정일</th></tr></thead>
        <tbody>
          {files.map((file) => {
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
  );
}
