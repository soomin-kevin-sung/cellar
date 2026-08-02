import { useEffect, useState, type ReactNode } from "react";
import {
  Archive,
  Bell,
  Clock3,
  FolderKanban,
  HardDrive,
  Menu,
  Search,
  Trash2,
  UploadCloud,
  X,
} from "lucide-react";

type AppShellProps = {
  children: ReactNode;
};

const navigation = [
  { label: "프로젝트", icon: FolderKanban, active: true },
  { label: "최근 항목", icon: Clock3 },
  { label: "전송", icon: UploadCloud, badge: "2" },
  { label: "휴지통", icon: Trash2 },
];

function Navigation({ mobile = false }: { mobile?: boolean }) {
  return (
    <nav aria-label={mobile ? "모바일 탐색" : "주 탐색"} className="shell-navigation">
      <p className="navigation-label">Workspace</p>
      <ul>
        {navigation.map(({ label, icon: Icon, active, badge }) => (
          <li key={label}>
            <button className={active ? "nav-item nav-item-active" : "nav-item"} type="button">
              <Icon aria-hidden="true" size={18} strokeWidth={1.8} />
              <span>{label}</span>
              {badge ? <span className="nav-badge">{badge}</span> : null}
            </button>
          </li>
        ))}
      </ul>
    </nav>
  );
}

export function AppShell({ children }: AppShellProps) {
  const [mobileOpen, setMobileOpen] = useState(false);

  useEffect(() => {
    if (!mobileOpen) return undefined;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMobileOpen(false);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [mobileOpen]);

  return (
    <div className="app-frame">
      <aside className="desktop-sidebar">
        <a className="brand" href="#top" aria-label="Cellar 홈">
          <span className="brand-mark"><Archive aria-hidden="true" size={19} /></span>
          <span className="brand-word">Cellar</span>
          <span className="brand-status" title="연결됨" />
        </a>
        <Navigation />
        <div className="storage-card">
          <div className="storage-heading">
            <span><HardDrive aria-hidden="true" size={16} /> 저장 공간</span>
            <strong>64%</strong>
          </div>
          <div className="storage-track"><span /></div>
          <p>1.28 TB / 2 TB</p>
        </div>
        <div className="account-card">
          <span className="avatar">KS</span>
          <span><strong>Kevin Sung</strong><small>Owner</small></span>
          <button type="button" aria-label="계정 메뉴">•••</button>
        </div>
      </aside>

      <section className="app-stage">
        <header className="topbar">
          <button className="icon-button mobile-menu-button" type="button" aria-label="메뉴 열기" onClick={() => setMobileOpen(true)}>
            <Menu aria-hidden="true" size={21} />
          </button>
          <div className="mobile-brand">Cellar</div>
          <label className="global-search">
            <Search aria-hidden="true" size={17} />
            <span className="sr-only">전체 검색</span>
            <input type="search" placeholder="파일과 프로젝트 검색" />
            <kbd>⌘ K</kbd>
          </label>
          <div className="topbar-actions">
            <span className="connection-pill"><i /> UI preview</span>
            <button className="icon-button" type="button" aria-label="알림"><Bell aria-hidden="true" size={19} /></button>
          </div>
        </header>
        <main id="top" tabIndex={-1}>{children}</main>
      </section>

      {mobileOpen ? (
        <div className="mobile-overlay" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget) setMobileOpen(false);
        }}>
          <aside className="mobile-drawer" aria-label="모바일 메뉴">
            <div className="drawer-heading">
              <a className="brand" href="#top"><span className="brand-mark"><Archive aria-hidden="true" size={19} /></span><span className="brand-word">Cellar</span></a>
              <button className="icon-button" type="button" aria-label="메뉴 닫기" onClick={() => setMobileOpen(false)}><X aria-hidden="true" size={21} /></button>
            </div>
            <Navigation mobile />
            <div className="storage-card drawer-storage">
              <div className="storage-heading"><span><HardDrive aria-hidden="true" size={16} /> 저장 공간</span><strong>64%</strong></div>
              <div className="storage-track"><span /></div>
              <p>1.28 TB / 2 TB</p>
            </div>
          </aside>
        </div>
      ) : null}
    </div>
  );
}
