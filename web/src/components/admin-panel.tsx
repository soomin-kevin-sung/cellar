import { KeyRound, UserPlus, Users } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";

import { api } from "../api";
import type { CurrentUser, ManagedUser, UserRole } from "../types";

export function AdminPanel({ currentUser }: { currentUser: CurrentUser }) {
  const [users, setUsers] = useState<ManagedUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState<UserRole>("member");
  const [creating, setCreating] = useState(false);
  const [resetUser, setResetUser] = useState<ManagedUser | null>(null);
  const [resetPassword, setResetPassword] = useState("");

  const load = async () => {
    setLoading(true);
    setError("");
    try { setUsers(await api.listUsers()); }
    catch { setError("사용자를 불러오지 못했습니다."); }
    finally { setLoading(false); }
  };

  useEffect(() => {
    const controller = new AbortController();
    api.listUsers(controller.signal).then(setUsers).catch((reason: unknown) => {
      if (reason instanceof DOMException && reason.name === "AbortError") return;
      setError("사용자를 불러오지 못했습니다.");
    }).finally(() => setLoading(false));
    return () => controller.abort();
  }, []);

  const create = async (event: FormEvent) => {
    event.preventDefault();
    setCreating(true);
    setError("");
    try {
      await api.createUser(username, password, role);
      setUsername(""); setPassword(""); setRole("member");
      await load();
    } catch { setError("계정을 만들지 못했습니다. 사용자 이름과 비밀번호를 확인해주세요."); }
    finally { setCreating(false); }
  };

  const update = async (user: ManagedUser, change: { active?: boolean; role?: UserRole }) => {
    setError("");
    try { await api.updateUser(user.id, change); await load(); }
    catch { setError("계정 변경사항을 저장하지 못했습니다."); }
  };

  const reset = async (event: FormEvent) => {
    event.preventDefault();
    if (!resetUser) return;
    try {
      await api.updateUser(resetUser.id, { password: resetPassword });
      setResetUser(null); setResetPassword("");
    } catch { setError("비밀번호를 변경하지 못했습니다."); }
  };

  return (
    <section className="admin-page" aria-labelledby="admin-title">
      <header className="admin-header">
        <div>
          <h1 id="admin-title">사용자</h1>
        </div>
        <div className="admin-count"><Users aria-hidden="true" size={18} /><strong>{users.filter((user) => user.active).length}</strong><span>활성</span></div>
      </header>

      <form className="admin-create" onSubmit={create}>
        <div className="admin-create__heading"><UserPlus aria-hidden="true" size={18} /><strong>사용자 추가</strong></div>
        <label><span>사용자 이름</span><input maxLength={32} minLength={3} onChange={(event) => setUsername(event.target.value)} pattern="[A-Za-z0-9._-]+" placeholder="kevin" required value={username} /></label>
        <label><span>임시 비밀번호</span><input minLength={10} onChange={(event) => setPassword(event.target.value)} placeholder="10자 이상" required type="password" value={password} /></label>
        <label><span>권한</span><select onChange={(event) => setRole(event.target.value as UserRole)} value={role}><option value="member">일반 사용자</option><option value="admin">관리자</option></select></label>
        <button className="button button--primary" disabled={creating} type="submit">{creating ? "추가 중…" : "추가"}</button>
      </form>

      {error ? <p className="admin-error" role="alert">{error}</p> : null}

      <div className="user-list" aria-busy={loading}>
        {loading ? <p className="inline-status">사용자 불러오는 중…</p> : users.map((user) => (
          <article className="user-row" key={user.id}>
            <div className="user-avatar" aria-hidden="true">{user.username.slice(0, 1).toUpperCase()}</div>
            <div className="user-row__identity"><strong>{user.username}</strong><span>{user.id === currentUser.id ? "나" : user.active ? "활성" : "비활성"}</span></div>
            <select aria-label={`${user.username} 권한`} disabled={user.id === currentUser.id} onChange={(event) => void update(user, { role: event.target.value as UserRole })} value={user.role}><option value="member">일반 사용자</option><option value="admin">관리자</option></select>
            <button className="button button--quiet" onClick={() => setResetUser(user)} type="button"><KeyRound aria-hidden="true" size={15} />비밀번호 변경</button>
            <button className={`button ${user.active ? "button--quiet button--danger" : "button--secondary"}`} disabled={user.id === currentUser.id} onClick={() => void update(user, { active: !user.active })} type="button">{user.active ? "비활성화" : "활성화"}</button>
          </article>
        ))}
      </div>

      {resetUser ? (
        <div className="reset-card">
          <form onSubmit={reset}>
            <div><strong>{resetUser.username} 비밀번호 변경</strong><span>기존 로그인은 해제됩니다.</span></div>
            <input aria-label="새 비밀번호" autoFocus minLength={10} onChange={(event) => setResetPassword(event.target.value)} placeholder="새 비밀번호" required type="password" value={resetPassword} />
            <button className="button button--primary" type="submit">저장</button>
            <button className="button button--quiet" onClick={() => { setResetUser(null); setResetPassword(""); }} type="button">취소</button>
          </form>
        </div>
      ) : null}
    </section>
  );
}
