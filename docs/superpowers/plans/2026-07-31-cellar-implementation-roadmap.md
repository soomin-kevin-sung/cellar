# Cellar Implementation Roadmap

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the approved Cellar v1 as a secure Windows service and responsive personal project-file hub.

**Architecture:** Build a Rust modular monolith with platform-neutral core contracts, Windows-specific filesystem and service adapters, SQLite persistence, and an embedded React application. Delivery is split into five testable phases so security and crash-safety foundations exist before file features and product polish.

**Tech Stack:** Rust 1.93+, Axum 0.8, Tokio 1.53, SQLx 0.9 with SQLite, windows/windows-service, React 19, TypeScript 7, Vite 8, TanStack Router/Query, Tailwind CSS 4, Vitest 4, Playwright 1.62.

---

## Source of Truth

- Approved design: `docs/superpowers/specs/2026-07-31-cellar-design.md`
- Target integration branch: `develop`
- Host baseline: Windows 11 x64, Rust 1.93+, Node.js 24+

## Plan Sequence

Implement these plans in order:

1. `docs/superpowers/plans/2026-07-31-cellar-phase-1-foundation.md`
2. `docs/superpowers/plans/2026-07-31-cellar-phase-2-core-files.md`
3. `docs/superpowers/plans/2026-07-31-cellar-phase-3-reconciliation.md`
4. `docs/superpowers/plans/2026-07-31-cellar-phase-4-web-ui.md`
5. `docs/superpowers/plans/2026-07-31-cellar-phase-5-release.md`

Do not start a phase until the prior phase's full verification command passes and its review findings are resolved.

## Locked File Map

```text
cellar/
├─ Cargo.toml
├─ rust-toolchain.toml
├─ crates/
│  ├─ cellar-core/
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ error.rs
│  │     ├─ ids.rs
│  │     ├─ project.rs
│  │     ├─ file_entry.rs
│  │     ├─ upload.rs
│  │     ├─ operation.rs
│  │     └─ ports.rs
│  ├─ cellar-config/
│  │  └─ src/{lib.rs,model.rs,validate.rs,store.rs}
│  ├─ cellar-db/
│  │  └─ src/{lib.rs,pool.rs,migrate.rs,project_repo.rs,file_repo.rs,upload_repo.rs,operation_repo.rs,backup.rs}
│  ├─ cellar-storage/
│  │  └─ src/{lib.rs,names.rs,ranges.rs,traits.rs}
│  ├─ cellar-windows/
│  │  └─ src/{lib.rs,handles.rs,identity.rs,names.rs,preflight.rs,watcher.rs,service.rs,acl.rs}
│  ├─ cellar-auth/
│  │  └─ src/{lib.rs,claims.rs,jwks.rs,middleware.rs,csrf.rs,enrollment.rs}
│  ├─ cellar-api/
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ router.rs
│  │     ├─ response.rs
│  │     ├─ health.rs
│  │     ├─ events.rs
│  │     └─ routes/{mod.rs,session.rs,projects.rs,files.rs,uploads.rs,trash.rs,preview.rs}
│  └─ cellar-service/
│     └─ src/{main.rs,app.rs,cli.rs,logging.rs,tls.rs,recovery.rs}
│  ├─ cellar-test-support/
│  │  └─ src/lib.rs
│  └─ cellar-e2e/
│     ├─ src/lib.rs
│     └─ tests/
├─ migrations/
│  ├─ 0001_initial.sql
│  └─ 0002_indexes.sql
├─ web/
│  ├─ package.json
│  ├─ vite.config.ts
│  ├─ playwright.config.ts
│  └─ src/
│     ├─ main.tsx
│     ├─ app/{router.tsx,providers.tsx,shell.tsx}
│     ├─ api/{client.ts,types.ts,sse.ts}
│     ├─ styles/{tokens.css,globals.css}
│     ├─ components/{button.tsx,dialog.tsx,progress.tsx,file-icon.tsx}
│     └─ features/
│        ├─ enrollment/
│        ├─ projects/
│        ├─ files/
│        ├─ transfers/
│        ├─ preview/
│        ├─ trash/
│        └─ settings/
├─ scripts/
│  ├─ build-release.ps1
│  ├─ install.ps1
│  ├─ update.ps1
│  ├─ uninstall.ps1
│  └─ smoke-cloudflare.ps1
├─ tests/
│  ├─ fixtures/
│  └─ windows-vm/
└─ docs/
   ├─ operations/
   └─ superpowers/
```

## Dependency Direction

```text
cellar-service
  -> cellar-api
  -> cellar-auth
  -> cellar-db
  -> cellar-windows
  -> cellar-config

cellar-api
  -> cellar-core ports
  -> cellar-auth

cellar-db
  -> cellar-core

cellar-windows
  -> cellar-storage
  -> cellar-core opaque platform identity

cellar-core
  -> no Axum, SQLx, Tokio runtime, or Windows dependency
```

Reject any change that introduces an inward dependency from core to an adapter.

## Shared Test Support Contract

`cellar-test-support` is a leaf-only test utility crate. Plans extend it as fixtures become available. It exposes:

```rust
pub struct TestDb;
pub struct TestService;
pub struct TestApi;
pub struct NtfsFixture;
pub struct AccessTokenBuilder;
pub struct FaultInjector;
```

Production crates never depend on it. Test snippets using `owner_app`, `fixture_watcher`, `crash_and_restart_upload`, or equivalent helpers must implement those helpers in the named test file or add a focused method to `cellar-test-support` in the same task.

## Specification Traceability

| Design section | Implementation coverage |
|---|---|
| Product goals and non-goals | Roadmap phase boundaries; all five phase gates |
| Architecture and modular monolith | Phase 1 Tasks 1–2 and dependency direction |
| Cloudflare, origin TLS, JWT, CSRF | Phase 1 Tasks 3, 5–8; Phase 5 Task 6 |
| NTFS and path security | Phase 1 Task 8; Phase 2 Task 2; Phase 3 Task 1 |
| Preview boundary | Phase 4 Task 7; Phase 5 security smoke |
| Storage layout and SQLite DDL | Phase 1 Tasks 3–4 |
| Project and file model | Phase 2 Tasks 1–3 |
| Upload and operation journal | Phase 2 Tasks 4–5 |
| Download HTTP contract | Phase 2 Task 6 |
| Mutation, trash, and recovery tables | Phase 2 Tasks 7–8 |
| Explorer watcher and reconciliation | Phase 3 Tasks 1–5 |
| Backup and recovered projects | Phase 3 Task 6 |
| API pagination, SSE, limits, logging | Phase 1 Task 8; Phase 2 Task 3; Phase 4 Task 3 |
| Responsive and accessible frontend | Phase 4 Tasks 1–7 |
| Windows install, update, uninstall | Phase 5 Tasks 2–5 |
| Acceptance, performance, current-PC install | Phase 5 Tasks 6–7 |

## Phase Gates

### Gate 1: Foundation

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit `0`; service starts with separate health and authenticated TLS listeners; owner enrollment and storage preflight tests pass.

### Gate 2: Core files

```powershell
cargo test --workspace
cargo test -p cellar-api --test project_files
cargo test -p cellar-api --test resumable_upload
cargo test -p cellar-api --test ranged_download
cargo test -p cellar-api --test mutation_recovery
```

Expected: all core project, upload, download, no-overwrite, trash, and crash-recovery tests pass.

### Gate 3: Reconciliation

```powershell
cargo test -p cellar-windows --test ntfs_identity
cargo test -p cellar-windows --test watcher_overflow
cargo test -p cellar-service --test reconcile_faults
cargo test -p cellar-db --test backup_restore
```

Expected: Explorer rename/move identity, dirty-epoch fencing, settling, hash queue, and restore reconciliation pass.

### Gate 4: Web UI

```powershell
npm --prefix web ci
npm --prefix web run lint
npm --prefix web run typecheck
npm --prefix web run test
npm --prefix web run build
npm --prefix web run test:e2e
```

Expected: unit, accessibility, Chromium, Firefox, WebKit, and responsive critical flows pass.

### Gate 5: Release

```powershell
pwsh -File scripts/build-release.ps1
pwsh -File tests/windows-vm/run.ps1
pwsh -File scripts/smoke-cloudflare.ps1
cargo test --workspace
npm --prefix web run test:e2e
```

Expected: signed-manifest release package, clean VM install/reboot/update/rollback/uninstall, real Cloudflare owner access, and current-PC smoke tests pass.

## Commit Policy

- Commit after every numbered task.
- Use `test:`, `feat:`, `fix:`, `build:`, or `docs:` prefixes.
- Stage only files listed by that task.
- Keep `develop` deployable at every phase gate.
- Do not push secrets, origin private keys, Cloudflare tokens, generated databases, storage fixtures, or `ProgramData` copies.
