import { FormEvent, useEffect, useState } from "react";

import App from "../app";
import { api, ApiError } from "../api";
import type { CurrentUser } from "../types";

type AuthState = "loading" | "guest" | "ready" | "error";

export function CellarRoot() {
  const [state, setState] = useState<AuthState>("loading");
  const [user, setUser] = useState<CurrentUser | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    api.me(controller.signal).then((current) => {
      setUser(current);
      setState("ready");
    }).catch((reason: unknown) => {
      if (reason instanceof DOMException && reason.name === "AbortError") return;
      setState(reason instanceof ApiError && reason.status === 401 ? "guest" : "error");
    });
    return () => controller.abort();
  }, []);

  if (state === "loading") {
    return <main className="auth-loading" aria-label="불러오는 중" />;
  }

  if (state === "error") {
    return (
      <main className="auth-error">
        <h1>접속할 수 없습니다</h1>
        <p>로그인 상태를 확인하지 못했습니다.</p>
        <button className="button button--secondary" onClick={() => window.location.reload()} type="button">새로고침</button>
      </main>
    );
  }

  if (state === "guest" || !user) {
    return <LoginPage onLogin={(current) => { setUser(current); setState("ready"); }} />;
  }

  return (
    <App
      currentUser={user}
      onLogout={async () => {
        await api.logout();
        setUser(null);
        setState("guest");
      }}
    />
  );
}

function LoginPage({ onLogin }: { onLogin: (user: CurrentUser) => void }) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [unlocking, setUnlocking] = useState(false);
  const [error, setError] = useState("");

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setSubmitting(true);
    setError("");
    let authenticated = false;
    try {
      const current = await api.login(username, password);
      authenticated = true;
      setUnlocking(true);
      const reducedMotion = typeof window.matchMedia === "function" &&
        window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      await new Promise((resolve) => window.setTimeout(resolve, reducedMotion ? 0 : 1150));
      onLogin(current);
    } catch (reason) {
      setError(reason instanceof ApiError && reason.status === 401
        ? "사용자 이름 또는 비밀번호가 올바르지 않습니다."
        : "지금은 로그인할 수 없습니다.");
    } finally {
      if (!authenticated) setSubmitting(false);
    }
  };

  return (
    <main className={`login-page${submitting ? " is-checking" : ""}${unlocking ? " is-unlocked" : ""}`}>
      <div className="login-orbit" aria-hidden="true" />
      <div className="login-beam" aria-hidden="true" />
      <section className="login-panel" aria-labelledby="login-title">
        <div className="login-panel__inner">
          <h1 className="sr-only" id="login-title">로그인</h1>

          <form className="login-form" onSubmit={submit}>
            <label>
              <span className="sr-only">사용자 이름</span>
              <input autoComplete="username" autoFocus maxLength={32} onChange={(event) => setUsername(event.target.value)} placeholder="사용자 이름" required value={username} />
            </label>
            <div className="login-password-field">
              <label>
                <span className="sr-only">비밀번호</span>
                <input autoComplete="current-password" minLength={10} onChange={(event) => setPassword(event.target.value)} placeholder="비밀번호" required type="password" value={password} />
              </label>
              <button aria-label="로그인" className="login-enter" disabled={submitting} type="submit">{submitting ? "…" : "→"}</button>
            </div>
            {error ? <p className="login-form__error" role="alert">{error}</p> : null}
          </form>
        </div>
      </section>
    </main>
  );
}
