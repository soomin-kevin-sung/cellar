import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";

import type { FormEvent, KeyboardEvent, MouseEvent, RefObject } from "react";
import type { Project } from "../types";

interface ProjectCreateDialogProps {
  open: boolean;
  onClose: () => void;
  onCreate: (name: string) => Promise<Project>;
  fallbackFocusRef?: RefObject<HTMLElement | null>;
}

function validateProjectName(value: string): string | null {
  const normalized = value.trim();
  if (normalized.length === 0) return "Enter a project name.";
  if ([...normalized].length > 100) return "Use 100 characters or fewer.";
  if (/\p{Cc}/u.test(normalized)) return "Project names cannot contain control characters.";
  return null;
}

export function ProjectCreateDialog({ fallbackFocusRef, open, onClose, onCreate }: ProjectCreateDialogProps) {
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  const submittingRef = useRef(false);
  const focusFallback = useCallback(() => fallbackFocusRef?.current?.focus(), [fallbackFocusRef]);

  useEffect(() => {
    if (!open) return;
    openerRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const dialog = dialogRef.current;
    if (!dialog) return;

    if (typeof dialog.showModal === "function") {
      if (!dialog.open) dialog.showModal();
    } else if (!dialog.open) {
      dialog.setAttribute("open", "");
    }
    inputRef.current?.focus();

    return () => {
      if (dialog.open) {
        if (typeof dialog.close === "function") dialog.close();
        else dialog.removeAttribute("open");
      }
      if (openerRef.current?.isConnected) openerRef.current.focus();
      else focusFallback();
      openerRef.current = null;
    };
  }, [focusFallback, open]);

  if (!open) return null;

  const close = () => {
    if (!pending) {
      setName("");
      setError(null);
      submittingRef.current = false;
      onClose();
    }
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLDialogElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      close();
    }
  };

  const handleBackdropClick = (event: MouseEvent<HTMLDialogElement>) => {
    if (event.target === event.currentTarget) close();
  };

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (submittingRef.current) return;

    const validationError = validateProjectName(name);
    if (validationError) {
      setError(validationError);
      inputRef.current?.focus();
      return;
    }

    submittingRef.current = true;
    setPending(true);
    setError(null);
    try {
      await onCreate(name.trim());
      setName("");
      setPending(false);
      submittingRef.current = false;
      onClose();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Project creation failed. Please try again.");
      submittingRef.current = false;
      setPending(false);
      inputRef.current?.focus();
    }
  };

  return (
    <dialog
      aria-labelledby="create-project-title"
      aria-modal="true"
      className="dialog"
      onCancel={(event) => { event.preventDefault(); close(); }}
      onClick={handleBackdropClick}
      onKeyDown={handleKeyDown}
      ref={dialogRef}
    >
      <form className="dialog__surface" onSubmit={handleSubmit}>
        <div className="dialog__header">
          <div>
            <p className="eyebrow">New workspace</p>
            <h2 id="create-project-title">Create project</h2>
          </div>
          <button className="icon-button" disabled={pending} onClick={close} type="button" aria-label="Close dialog">
            <X aria-hidden="true" size={19} />
          </button>
        </div>

        <div className="field">
          <label htmlFor="project-name">Project name</label>
          <input
            aria-describedby="project-name-hint"
            autoComplete="off"
            disabled={pending}
            id="project-name"
            maxLength={202}
            onChange={(event) => { setName(event.target.value); setError(null); }}
            ref={inputRef}
            value={name}
          />
          <p id="project-name-hint">Use a clear name for the files you’ll keep together.</p>
          {error ? <p className="field__error" role="alert">{error}</p> : null}
        </div>

        <div className="dialog__actions">
          <button className="button button--secondary" disabled={pending} onClick={close} type="button">Cancel</button>
          <button className="button button--primary" disabled={pending} type="submit">
            {pending ? "Creating project" : "Create project"}
          </button>
        </div>
      </form>
    </dialog>
  );
}
