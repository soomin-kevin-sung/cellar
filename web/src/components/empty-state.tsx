import { FolderPlus } from "lucide-react";

interface EmptyStateProps {
  onCreateProject: () => void;
}

export function EmptyState({ onCreateProject }: EmptyStateProps) {
  return (
    <section className="empty-state" aria-labelledby="empty-title">
      <span className="empty-state__icon" aria-hidden="true">
        <FolderPlus size={24} strokeWidth={1.75} />
      </span>
      <h1 id="empty-title">Create your first project</h1>
      <p>Projects keep related files together in one focused workspace.</p>
      <button className="button button--primary" type="button" onClick={onCreateProject}>
        Create project
      </button>
    </section>
  );
}
