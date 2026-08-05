import { Check, RotateCcw, Upload, X } from "lucide-react";
import type { CSSProperties } from "react";

import type { UploadTask } from "./upload-panel";

export function UploadDock({
  task,
  onOpen,
  onReset,
  onRetry,
}: {
  task: UploadTask;
  onOpen: () => void;
  onReset: () => void;
  onRetry: () => void;
}) {
  return (
    <aside aria-label="백그라운드 업로드" className={`upload-dock upload-dock--${task.state}`}>
      <button className="upload-dock__main" onClick={onOpen} type="button">
        <span className="upload-dock__icon" aria-hidden="true">
          {task.state === "complete" ? <Check size={16} /> : <Upload size={16} />}
        </span>
        <span className="upload-dock__copy">
          <strong>{task.file.name}</strong>
          <span>{task.projectName} · {task.state === "uploading" ? `${task.progress}%` : task.state === "complete" ? "저장됨" : "실패"}</span>
        </span>
      </button>
      {task.state === "error" ? (
        <button aria-label="업로드 다시 시도" className="upload-dock__action" onClick={onRetry} type="button"><RotateCcw size={15} /></button>
      ) : null}
      {task.state !== "uploading" ? (
        <button aria-label="업로드 상태 닫기" className="upload-dock__action" onClick={onReset} type="button"><X size={15} /></button>
      ) : null}
      <span className="upload-dock__progress" style={{ "--upload-progress": `${task.progress}%` } as CSSProperties} />
    </aside>
  );
}
