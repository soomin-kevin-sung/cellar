import { Upload, X } from "lucide-react";
import { forwardRef, useEffect, useRef, useState, type ChangeEvent, type DragEvent } from "react";
import {
  UploadPausedError,
  UploadRetryableError,
  clearStoredUpload,
  loadStoredUpload,
  matchesStoredUpload,
  uploadFile,
  type StoredUpload,
  type UploadSession,
} from "../upload-client";
import { formatBinarySize } from "../file-format";

interface UploadPanelProps {
  projectId: string;
  projectName: string;
  onComplete: () => void | Promise<void>;
  upload?: typeof uploadFile;
}

type PanelState = "idle" | "uploading" | "paused" | "retryable" | "complete" | "error";

function recoveredSession(metadata: StoredUpload): UploadSession {
  return {
    id: metadata.uploadId,
    projectId: metadata.projectId,
    fileName: metadata.fileName,
    totalSize: metadata.totalSize,
    committedOffset: metadata.committedOffset,
    state: "active",
  };
}

function safeMessage(reason: unknown) {
  return reason instanceof Error && reason.message.trim()
    ? reason.message
    : "The upload could not continue safely.";
}

export const UploadPanel = forwardRef<HTMLElement, UploadPanelProps>(function UploadPanel(
  { projectId, projectName, onComplete, upload = uploadFile }, ref,
) {
  const [recovered, setRecovered] = useState<StoredUpload | null>(() => loadStoredUpload());
  const [state, setState] = useState<PanelState>("idle");
  const [file, setFile] = useState<File | null>(null);
  const [session, setSession] = useState<UploadSession | undefined>();
  const [committed, setCommitted] = useState(() => recovered ? Number(recovered.committedOffset) : 0);
  const [total, setTotal] = useState(() => recovered ? Number(recovered.totalSize) : 0);
  const [message, setMessage] = useState("");
  const [dragging, setDragging] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const uploadButtonRef = useRef<HTMLButtonElement>(null);
  const controllerRef = useRef<AbortController | null>(null);
  const restoreFocusRef = useRef(false);
  const busy = state === "uploading";

  useEffect(() => () => controllerRef.current?.abort(), []);
  useEffect(() => {
    if (restoreFocusRef.current && state === "idle" && !recovered) {
      uploadButtonRef.current?.focus();
      restoreFocusRef.current = false;
    }
  }, [recovered, state]);

  const run = async (selected: File, resume?: UploadSession) => {
    const scopedProjectId = resume?.projectId ?? projectId;
    setFile(selected);
    setSession(resume);
    setState("uploading");
    setMessage("");
    setTotal(selected.size);
    setCommitted(resume ? Number(resume.committedOffset) : 0);
    const controller = new AbortController();
    controllerRef.current = controller;
    try {
      const completed = await upload({
        projectId: scopedProjectId,
        file: selected,
        session: resume,
        signal: controller.signal,
        onProgress: (progress) => {
          setCommitted(progress.committed);
          setTotal(progress.total);
          setSession(progress.session);
        },
      });
      setSession(completed);
      setCommitted(Number(completed.committedOffset));
      setTotal(Number(completed.totalSize));
      setRecovered(null);
      setState("complete");
      await onComplete();
    } catch (reason) {
      if (controller.signal.aborted) return;
      if (reason instanceof UploadPausedError) {
        setSession(reason.session);
        setCommitted(Number(reason.session.committedOffset));
        setState("paused");
      } else if (reason instanceof UploadRetryableError) {
        setSession(reason.session);
        setCommitted(Number(reason.session.committedOffset));
        setTotal(Number(reason.session.totalSize));
        setState("retryable");
      } else {
        setState("error");
      }
      setMessage(safeMessage(reason));
    } finally {
      if (controllerRef.current === controller) controllerRef.current = null;
    }
  };

  const selectFile = (selected?: File) => {
    if (!selected || busy) return;
    if (recovered) {
      if (!matchesStoredUpload(recovered, projectId, selected)) {
        setState("error");
        setMessage(recovered.projectId !== projectId
          ? "This paused upload belongs to another project. Select that project before choosing the file."
          : "The selected file does not match the paused upload. Choose the same name and size.");
        return;
      }
      setRecovered(null);
      void run(selected, recoveredSession(recovered));
      return;
    }
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

  const discard = () => {
    restoreFocusRef.current = true;
    controllerRef.current?.abort();
    clearStoredUpload();
    setRecovered(null);
    setFile(null);
    setSession(undefined);
    setCommitted(0);
    setTotal(0);
    setMessage("");
    setState("idle");
  };

  const percentage = total === 0 ? (state === "complete" ? 100 : 0) : Math.min(100, Math.round((committed / total) * 100));
  const sessionProject = recovered?.projectId ?? session?.projectId ?? projectId;
  const recoveryElsewhere = Boolean(recovered && recovered.projectId !== projectId);

  return (
    <section aria-labelledby="upload-title" className="upload-panel" ref={ref} tabIndex={-1}>
      <div className="upload-panel__heading">
        <div><p className="eyebrow">Transfer</p><h2 id="upload-title">Upload a file</h2></div>
        {state === "uploading" ? (
          <button className="button button--quiet" onClick={discard} type="button">
            <X aria-hidden="true" size={15} />Cancel upload
          </button>
        ) : null}
      </div>

      <input
        aria-label="Choose a file to upload"
        className="sr-only"
        disabled={busy}
        onChange={inputChanged}
        ref={inputRef}
        type="file"
      />

      {recovered ? (
        <div className="recovery-card" role="status">
          <div>
            <strong>Paused upload found</strong>
            <p>
              {recoveryElsewhere
                ? `${recovered.fileName} belongs to another project (${recovered.projectId}).`
                : `Choose ${recovered.fileName} again to continue at ${formatBinarySize(recovered.committedOffset)}.`}
            </p>
          </div>
          <div className="recovery-card__actions">
            <button className="button button--secondary" disabled={recoveryElsewhere} onClick={() => inputRef.current?.click()} type="button">
              Choose same file
            </button>
            <button aria-label="Dismiss recovered upload" className="button button--quiet" onClick={discard} type="button">Dismiss</button>
          </div>
        </div>
      ) : null}

      {!recovered && state === "idle" ? (
        <div
          className={`upload-drop-target${dragging ? " is-dragging" : ""}`}
          data-testid="upload-drop-target"
          onDragEnter={(event) => { event.preventDefault(); if (!busy) setDragging(true); }}
          onDragLeave={() => setDragging(false)}
          onDragOver={(event) => event.preventDefault()}
          onDrop={drop}
        >
          <Upload aria-hidden="true" size={21} strokeWidth={1.7} />
          <div><strong>Drop one file here</strong><p>or choose it from this device</p></div>
          <button className="button button--primary" onClick={() => inputRef.current?.click()} ref={uploadButtonRef} type="button">Upload file</button>
        </div>
      ) : null}

      {state !== "idle" && (file || session || message) ? (
        <div className="upload-status" aria-live="polite">
          <div className="upload-status__line">
            <div><strong>{file?.name ?? session?.fileName}</strong><span>{sessionProject === projectId ? projectName : `Project ${sessionProject}`}</span></div>
            <span>{percentage}%</span>
          </div>
          <progress aria-label="Upload progress" aria-valuenow={percentage} max={100} value={percentage}>{percentage}%</progress>
          <div className="upload-status__meta">
            <span>{formatBinarySize(String(committed))} of {formatBinarySize(String(total))}</span>
            <span role="status">{state === "uploading" ? "Uploading" : state === "complete" ? "Upload complete" : state === "paused" ? "Paused" : state === "retryable" ? "Waiting to finalize" : "Needs attention"}</span>
          </div>
          {message ? <p className="upload-message" role="alert">{message}</p> : null}
          {(state === "paused" || state === "retryable") && file && session ? (
            <div className="upload-status__actions">
              <button className="button button--secondary" onClick={() => void run(file, session)} type="button">Retry upload</button>
              <button className="button button--quiet" onClick={discard} type="button">Discard upload</button>
            </div>
          ) : null}
          {state === "error" && !recovered ? (
            <button className="button button--quiet" onClick={discard} type="button">Dismiss</button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
});
