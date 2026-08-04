import { Upload } from "lucide-react";
import { forwardRef, useEffect, useRef, useState, type ChangeEvent, type DragEvent } from "react";

import { formatBinarySize } from "../file-format";
import { uploadFile, type UploadResult } from "../upload-client";

interface UploadPanelProps {
  projectId: string;
  projectName: string;
  onComplete: () => void | Promise<void>;
  upload?: typeof uploadFile;
}

type PanelState = "idle" | "uploading" | "complete" | "error";

function safeMessage(reason: unknown) {
  return reason instanceof Error && reason.message.trim()
    ? reason.message
    : "파일을 업로드하지 못했습니다. 다시 시도해주세요.";
}

export const UploadPanel = forwardRef<HTMLElement, UploadPanelProps>(function UploadPanel(
  { projectId, projectName, onComplete, upload = uploadFile }, ref,
) {
  const [state, setState] = useState<PanelState>("idle");
  const [file, setFile] = useState<File | null>(null);
  const [result, setResult] = useState<UploadResult | null>(null);
  const [message, setMessage] = useState("");
  const [dragging, setDragging] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const controllerRef = useRef<AbortController | null>(null);
  const busy = state === "uploading";

  useEffect(() => () => controllerRef.current?.abort(), []);

  const run = async (selected: File) => {
    setFile(selected);
    setResult(null);
    setMessage("");
    setState("uploading");
    const controller = new AbortController();
    controllerRef.current = controller;
    try {
      const uploaded = await upload({ projectId, file: selected, signal: controller.signal });
      setResult(uploaded);
      setState("complete");
      await onComplete();
    } catch (reason) {
      if (controller.signal.aborted) return;
      setMessage(safeMessage(reason));
      setState("error");
    } finally {
      if (controllerRef.current === controller) controllerRef.current = null;
    }
  };

  const selectFile = (selected?: File) => {
    if (!selected || busy) return;
    void run(selected);
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

  const reset = () => {
    setFile(null);
    setResult(null);
    setMessage("");
    setState("idle");
  };

  return (
    <section aria-labelledby="upload-title" className="upload-panel" ref={ref} tabIndex={-1}>
      <div className="upload-panel__heading">
        <div><p className="eyebrow">업로드</p><h2 id="upload-title">파일 올리기</h2></div>
      </div>

      <input
        aria-label="업로드할 파일 선택"
        className="sr-only"
        disabled={busy}
        onChange={inputChanged}
        ref={inputRef}
        type="file"
      />

      {state === "idle" ? (
        <div
          className={`upload-drop-target${dragging ? " is-dragging" : ""}`}
          data-testid="upload-drop-target"
          onDragEnter={(event) => { event.preventDefault(); setDragging(true); }}
          onDragLeave={() => setDragging(false)}
          onDragOver={(event) => event.preventDefault()}
          onDrop={drop}
        >
          <Upload aria-hidden="true" size={21} strokeWidth={1.7} />
          <div><strong>파일 하나를 여기에 놓으세요</strong><p>또는 이 PC에서 선택하세요</p></div>
          <button className="button button--primary" onClick={() => inputRef.current?.click()} type="button">파일 선택</button>
        </div>
      ) : null}

      {state !== "idle" && file ? (
        <div className="upload-status" aria-live="polite">
          <div className="upload-status__line">
            <div><strong>{file.name}</strong><span>{projectName}</span></div>
            <span>{formatBinarySize(String(result ? Number(result.size) : file.size))}</span>
          </div>
          <progress aria-label="업로드 진행률" max={100} value={state === "complete" ? 100 : undefined} />
          <div className="upload-status__meta">
            <span>{state === "uploading" ? "파일을 보내는 중" : state === "complete" ? "이 PC에 저장됨" : "업로드 실패"}</span>
            <span role="status">{state === "uploading" ? "업로드 중" : state === "complete" ? "업로드 완료" : "확인 필요"}</span>
          </div>
          {message ? <p className="upload-message" role="alert">{message}</p> : null}
          {state === "error" ? (
            <div className="upload-status__actions">
              <button className="button button--secondary" onClick={() => void run(file)} type="button">다시 업로드</button>
              <button className="button button--quiet" onClick={reset} type="button">다른 파일 선택</button>
            </div>
          ) : null}
          {state === "complete" ? (
            <button className="button button--quiet" onClick={reset} type="button">다른 파일 업로드</button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
});
