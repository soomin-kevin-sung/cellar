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
  if (state === "loading") return <p className="inline-status" role="status">Loading files…</p>;
  if (state === "error") {
    return (
      <div className="files-error">
        <p className="inline-error" role="alert">{error || "Files could not be loaded. Please try again."}</p>
        <button aria-label="Retry loading files" className="button button--secondary" onClick={onRetry} type="button">
          <RefreshCw aria-hidden="true" size={16} />Retry
        </button>
      </div>
    );
  }
  if (files.length === 0) {
    return (
      <div className="files-empty">
        <div><h2>No files in this project yet.</h2><p>Choose a file below to start an upload.</p></div>
      </div>
    );
  }
  return (
    <div className="file-table-wrap">
      <table aria-label="Project files" className="file-table">
        <thead><tr><th scope="col">Name</th><th scope="col">Size</th><th scope="col">Modified</th></tr></thead>
        <tbody>
          {files.map((file) => {
            const formattedTime = formatModifiedTime(file.modifiedAt);
            const validTime = formattedTime !== file.modifiedAt;
            return (
              <tr key={file.name}>
                <th data-label="Name" scope="row">
                  <a href={`/api/v1/projects/${encodeURIComponent(projectId)}/files/${encodeURIComponent(file.name)}`}>
                    {file.name}
                  </a>
                </th>
                <td data-label="Size">{formatBinarySize(file.size)}</td>
                <td data-label="Modified">{validTime ? <time dateTime={file.modifiedAt}>{formattedTime}</time> : file.modifiedAt}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
