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
      <h1 id="empty-title">첫 프로젝트를 만들어보세요</h1>
      <p>관련 파일을 프로젝트별로 모아둘 수 있습니다.</p>
      <button className="button button--primary" type="button" onClick={onCreateProject}>
        프로젝트 만들기
      </button>
    </section>
  );
}
