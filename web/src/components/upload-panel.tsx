import { Upload } from "lucide-react";
import { forwardRef, useRef, useState, type ChangeEvent, type DragEvent } from "react";

import { formatBinarySize } from "../file-format";
import type { UploadResult } from "../upload-client";

export type UploadTaskState = "uploading" | "complete" | "error";

export interface UploadTask {
  projectId: string;
  projectName: string;
  file: File;
  state: UploadTaskState;
  progress: number;
  result: UploadResult | null;
  message: string;
}

interface UploadPanelProps {
  projectId: string;
  projectName: string;
  task: UploadTask | null;
  onUpload: (file: File) => void;
  onRetry: () => void;
  onReset: () => void;
}

export const UploadPanel = forwardRef<HTMLElement, UploadPanelProps>(function UploadPanel(
  { projectId, projectName, task, onUpload, onRetry, onReset }, ref,
) {
  const [dragging, setDragging] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const busy = task?.state === "uploading";

  const selectFile = (selected?: File) => {
    if (!selected || busy) return;
    onUpload(selected);
  };

  const inputChanged = (event: ChangeEvent<HTMLInputElement>) => {
    selectFile(event.target.files?.[0]);
    event.target.value = "";
  };

  const drop = (event: DragEvent<HTMLElement>) => {
    event.preventDefault();
    setDragging(false);
    selectFile(event.dataTransfer.files[0]);
  };

  return (
    <section aria-labelledby="upload-title" className="upload-panel" ref={ref} tabIndex={-1}>
      <div className="upload-panel__heading">
        <div><p className="eyebrow">업로드</p><h2 id="upload-title">파일 올리기</h2></div>
        <span>{projectName}</span>
      </div>

      <input
        aria-label="업로드할 파일 선택"
        className="sr-only"
        disabled={busy}
        onChange={inputChanged}
        ref={inputRef}
        type="file"
      />

      {!task ? (
        <div
          className={`upload-drop-target${dragging ? " is-dragging" : ""}`}
          data-testid="upload-drop-target"
          onDragEnter={(event) => { event.preventDefault(); setDragging(true); }}
          onDragLeave={() => setDragging(false)}
          onDragOver={(event) => event.preventDefault()}
          onDrop={drop}
        >
          <Upload aria-hidden="true" size={21} strokeWidth={1.7} />
          <div><strong>파일 하나를 여기에 놓으세요</strong><p>{projectName} 프로젝트에 저장합니다.</p></div>
          <button className="button button--primary" onClick={() => inputRef.current?.click()} type="button">파일 선택</button>
        </div>
      ) : (
        <div className="upload-status" aria-live="polite">
          <div className="upload-status__line">
            <div><strong>{task.file.name}</strong><span>{task.projectName}</span></div>
            <span>{formatBinarySize(String(task.result ? Number(task.result.size) : task.file.size))}</span>
          </div>
          <progress aria-label="업로드 진행률" max={100} value={task.progress} />
          <div className="upload-status__meta">
            <span>{task.state === "uploading" ? "다른 화면에서도 계속 전송됩니다" : task.state === "complete" ? "이 PC에 저장됨" : "업로드 실패"}</span>
            <span role="status">{task.state === "uploading" ? `업로드 중 ${task.progress}%` : task.state === "complete" ? "업로드 완료" : "확인 필요"}</span>
          </div>
          {task.message ? <p className="upload-message" role="alert">{task.message}</p> : null}
          {task.state === "error" ? (
            <div className="upload-status__actions">
              <button className="button button--secondary" onClick={onRetry} type="button">처음부터 다시 업로드</button>
              <button className="button button--quiet" onClick={onReset} type="button">다른 파일 선택</button>
            </div>
          ) : null}
          {task.state === "complete" ? (
            <button className="button button--quiet" onClick={onReset} type="button">다른 파일 업로드</button>
          ) : null}
          {task.projectId !== projectId ? <p className="upload-background-note">현재 보고 있는 프로젝트와 관계없이 전송을 계속합니다.</p> : null}
        </div>
      )}
    </section>
  );
});
