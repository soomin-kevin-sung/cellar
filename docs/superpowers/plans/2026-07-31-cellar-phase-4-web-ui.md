# Cellar Phase 4 Web UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the polished, responsive, accessible Cellar project and file experience on the completed authenticated API.

**Architecture:** Use route-level feature modules, TanStack Query for server snapshots, a small transfer store for browser-held `File` objects, and SSE as an invalidation signal rather than an alternate source of truth. Keep dangerous content out of the DOM and make every drag or pointer interaction available by keyboard and standard controls.

**Tech Stack:** React 19, TypeScript 7, Vite 8, TanStack Router 1, TanStack Query 5, Tailwind CSS 4, Radix primitives, Vitest 4, Testing Library, Playwright 1.62.

---

### Task 1: Scaffold the web application and quality gates

**Files:**
- Create: `web/package.json`
- Create: `web/tsconfig.json`
- Create: `web/vite.config.ts`
- Create: `web/eslint.config.js`
- Create: `web/vitest.config.ts`
- Create: `web/playwright.config.ts`
- Create: `web/src/main.tsx`
- Create: `web/src/app/providers.tsx`

- [ ] **Step 1: Create the Vite React project**

```powershell
npm create vite@latest web -- --template react-ts
npm --prefix web install
npm --prefix web install @tanstack/react-router @tanstack/react-query @radix-ui/react-dialog
npm --prefix web install -D tailwindcss vitest @testing-library/react @testing-library/jest-dom @playwright/test eslint
```

- [ ] **Step 2: Lock scripts**

```json
{
  "scripts": {
    "dev": "vite",
    "build": "tsc -b && vite build",
    "lint": "eslint .",
    "typecheck": "tsc -b --pretty false",
    "test": "vitest run",
    "test:e2e": "playwright test"
  }
}
```

- [ ] **Step 3: Add providers**

```tsx
const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 15_000, retry: 1, refetchOnWindowFocus: true },
    mutations: { retry: false },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
```

- [ ] **Step 4: Run scaffold checks**

Run: `npm --prefix web run lint; npm --prefix web run typecheck; npm --prefix web run test; npm --prefix web run build`

Expected: all commands exit `0`.

- [ ] **Step 5: Commit**

```powershell
git add web
git commit -m "build: scaffold React application"
```

### Task 2: Build design tokens and the responsive application shell

**Files:**
- Create: `web/src/styles/tokens.css`
- Create: `web/src/styles/globals.css`
- Create: `web/src/app/shell.tsx`
- Create: `web/src/components/button.tsx`
- Create: `web/src/components/dialog.tsx`
- Test: `web/src/app/shell.test.tsx`

- [ ] **Step 1: Write failing shell accessibility test**

```tsx
it("provides labeled navigation and visible main landmark", () => {
  render(<AppShell><div>Content</div></AppShell>);
  expect(screen.getByRole("navigation", { name: /primary/i })).toBeVisible();
  expect(screen.getByRole("main")).toHaveTextContent("Content");
});
```

- [ ] **Step 2: Verify failure**

Run: `npm --prefix web run test -- shell.test.tsx`

Expected: FAIL.

- [ ] **Step 3: Define distinctive Cellar tokens**

```css
:root {
  --cellar-wine-950: #241017;
  --cellar-wine-700: #6f263d;
  --cellar-wine-500: #a34764;
  --cellar-slate-950: #111419;
  --cellar-slate-800: #252a32;
  --cellar-paper: #f5f1ec;
  --cellar-focus: #d77a96;
  --radius-card: 1.125rem;
  --shadow-float: 0 18px 60px rgb(17 20 25 / 18%);
}
```

Implement desktop sidebar, mobile sheet navigation, storage meter, status indicator, main landmark, 44-pixel targets, focus-visible rings, and reduced-motion media query.

- [ ] **Step 4: Run shell tests**

Run: `npm --prefix web run test -- shell.test.tsx`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add web/src/styles web/src/app/shell.tsx web/src/components
git commit -m "feat: add Cellar application shell"
```

### Task 3: Add typed API, enrollment, session, and connection state

**Files:**
- Create: `web/src/api/types.ts`
- Create: `web/src/api/client.ts`
- Create: `web/src/api/sse.ts`
- Create: `web/src/features/enrollment/enrollment-page.tsx`
- Create: `web/src/features/enrollment/session-provider.tsx`
- Test: `web/src/features/enrollment/enrollment.test.tsx`

- [ ] **Step 1: Write failing enrollment tests**

```tsx
it("claims once and then loads the owner session", async () => {
  server.use(claimSucceedsOnce(), sessionReturnsCsrf());
  render(<EnrollmentPage />);
  await user.type(screen.getByLabelText(/claim code/i), "correct-code");
  await user.click(screen.getByRole("button", { name: /claim cellar/i }));
  expect(await screen.findByText(/owner connected/i)).toBeVisible();
});
```

- [ ] **Step 2: Verify failure**

Run: `npm --prefix web run test -- enrollment.test.tsx`

Expected: FAIL.

- [ ] **Step 3: Implement authenticated client**

```ts
export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (csrfToken && !["GET", "HEAD"].includes(init.method ?? "GET")) {
    headers.set("X-Cellar-CSRF", csrfToken);
  }
  const response = await fetch(`/api/v1${path}`, { ...init, headers, credentials: "include" });
  if (!response.ok) throw await CellarApiError.fromResponse(response);
  return response.status === 204 ? (undefined as T) : response.json();
}
```

Implement unenrolled claim form, session bootstrap, CSRF rotation refresh, read-only offline/degraded banner, SSE monotonic IDs, reset invalidation, exponential backoff, and polling fallback.

- [ ] **Step 4: Run auth UI tests**

Run: `npm --prefix web run test -- enrollment.test.tsx`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add web/src/api web/src/features/enrollment
git commit -m "feat: add owner session experience"
```

### Task 4: Build project catalog and project detail

**Files:**
- Create: `web/src/features/projects/project-list.tsx`
- Create: `web/src/features/projects/project-card.tsx`
- Create: `web/src/features/projects/project-form.tsx`
- Create: `web/src/features/projects/project-header.tsx`
- Test: `web/src/features/projects/projects.test.tsx`
- E2E: `web/e2e/projects.spec.ts`

- [ ] **Step 1: Write failing project flow test**

```tsx
it("creates, edits, and archives a project", async () => {
  renderProjects();
  await user.click(screen.getByRole("button", { name: /new project/i }));
  await user.type(screen.getByLabelText(/name/i), "Cellar");
  await user.click(screen.getByRole("button", { name: /^create$/i }));
  expect(await screen.findByRole("heading", { name: "Cellar" })).toBeVisible();
});
```

- [ ] **Step 2: Verify failure**

Run: `npm --prefix web run test -- projects.test.tsx`

Expected: FAIL.

- [ ] **Step 3: Implement catalog views**

```tsx
export function ProjectCard({ project }: { project: ProjectSummary }) {
  return (
    <Link to="/projects/$projectId" params={{ projectId: project.id }}
      className="group rounded-[var(--radius-card)] bg-white/80 shadow-sm">
      <ProjectCover project={project} />
      <h2>{project.name}</h2>
      <p>{formatBytes(project.totalSize)} · {project.fileCount} files</p>
    </Link>
  );
}
```

Add card/list toggle, active/archive filter, search over project metadata only, cover, storage usage, optimistic version conflict refresh, and copied local path.

- [ ] **Step 4: Run unit and E2E tests**

Run: `npm --prefix web run test -- projects.test.tsx; npm --prefix web run test:e2e -- projects.spec.ts`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add web/src/features/projects web/e2e/projects.spec.ts
git commit -m "feat: add project catalog UI"
```

### Task 5: Build the paginated file browser and safe mutations

**Files:**
- Create: `web/src/features/files/file-browser.tsx`
- Create: `web/src/features/files/file-table.tsx`
- Create: `web/src/features/files/file-grid.tsx`
- Create: `web/src/features/files/file-actions.tsx`
- Create: `web/src/components/file-icon.tsx`
- Test: `web/src/features/files/file-browser.test.tsx`
- E2E: `web/e2e/files.spec.ts`

- [ ] **Step 1: Write failing browse and conflict tests**

```tsx
it("refreshes instead of overwriting on a 409 conflict", async () => {
  renderFileBrowser();
  server.use(renameReturnsConflict());
  await renameVisibleFile("report.pdf", "final.pdf");
  expect(await screen.findByText(/changed outside cellar/i)).toBeVisible();
  expect(invalidateFilesQuery).toHaveBeenCalled();
});
```

- [ ] **Step 2: Verify failure**

Run: `npm --prefix web run test -- file-browser.test.tsx`

Expected: FAIL.

- [ ] **Step 3: Implement browser views and actions**

```tsx
const filesQuery = useInfiniteQuery({
  queryKey: ["files", projectId, parentId, sort],
  queryFn: ({ pageParam }) => listFiles(projectId, parentId, pageParam),
  getNextPageParam: page => page.nextCursor,
  initialPageParam: null as string | null,
});
```

Implement breadcrumbs, virtualized or paginated table, grid, new folder, rename, move, copy, download, trash, explicit destructive confirmation, keyboard menus, and conflict refresh. Never expose an overwrite option.

- [ ] **Step 4: Run file UI tests**

Run: `npm --prefix web run test -- file-browser.test.tsx; npm --prefix web run test:e2e -- files.spec.ts`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add web/src/features/files web/src/components/file-icon.tsx web/e2e/files.spec.ts
git commit -m "feat: add project file browser"
```

### Task 6: Add resumable transfer UI

**Files:**
- Create: `web/src/features/transfers/transfer-store.ts`
- Create: `web/src/features/transfers/uploader.ts`
- Create: `web/src/features/transfers/transfer-panel.tsx`
- Test: `web/src/features/transfers/uploader.test.ts`
- E2E: `web/e2e/uploads.spec.ts`

- [ ] **Step 1: Write failing resume test**

```ts
it("continues from server committed offset", async () => {
  const file = fakeFile(80 * MiB);
  serverCommittedOffset = 32 * MiB;
  await resumeUpload(session, file);
  expect(sentOffsets).toEqual([32 * MiB, 64 * MiB]);
});
```

- [ ] **Step 2: Verify failure**

Run: `npm --prefix web run test -- uploader.test.ts`

Expected: FAIL.

- [ ] **Step 3: Implement uploader**

```ts
while (offset < file.size) {
  const chunk = file.slice(offset, Math.min(offset + maxChunkSize, file.size));
  const digest = await sha256(chunk);
  const status = await putChunk(session.id, offset.toString(), chunk, digest, signal);
  const committed = BigInt(status.committedOffset);
  if (committed > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("Browser cannot represent the committed file offset safely.");
  }
  offset = Number(committed);
  onProgress(offset / file.size);
}
await finalizeUpload(session.id);
```

Keep at most three file uploads active, preserve sessions across navigation, announce progress, cancel on user action, and after reload require reselection plus name/size/mtime/sampled-content verification.

- [ ] **Step 4: Run transfer tests**

Run: `npm --prefix web run test -- uploader.test.ts; npm --prefix web run test:e2e -- uploads.spec.ts`

Expected: PASS for interruption, retry, conflict, expiry, disk full, reload reselection, zero-byte, and screen-reader status.

- [ ] **Step 5: Commit**

```powershell
git add web/src/features/transfers web/e2e/uploads.spec.ts
git commit -m "feat: add resumable transfer UI"
```

### Task 7: Add preview, trash, settings, responsive and accessibility completion

**Files:**
- Create: `web/src/features/preview/preview-panel.tsx`
- Create: `web/src/features/trash/trash-page.tsx`
- Create: `web/src/features/settings/settings-page.tsx`
- Create: `web/src/features/projects/recent-items.tsx`
- E2E: `web/e2e/preview-security.spec.ts`
- E2E: `web/e2e/accessibility.spec.ts`
- E2E: `web/e2e/responsive.spec.ts`

- [ ] **Step 1: Write failing preview-security E2E test**

```ts
test("unsafe types never render inline", async ({ page }) => {
  await openFile(page, "payload.svg");
  await expect(page.getByText(/preview unavailable/i)).toBeVisible();
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: /download/i }).click();
  expect((await download).suggestedFilename()).toBe("payload.svg");
});
```

- [ ] **Step 2: Verify failure**

Run: `npm --prefix web run test:e2e -- preview-security.spec.ts accessibility.spec.ts responsive.spec.ts`

Expected: FAIL.

- [ ] **Step 3: Implement allowlisted preview and remaining screens**

```tsx
export function PreviewPanel({ file }: { file: FileEntry }) {
  if (!file.previewKind) return <DownloadOnly file={file} />;
  if (file.previewKind === "text") return <EscapedTextPreview file={file} maxBytes={2 * MiB} />;
  if (file.previewKind === "pdf") return <iframe sandbox="" src={file.previewUrl} title={file.name} />;
  return <NativeMediaPreview file={file} />;
}
```

Add trash restore/purge, recent metadata items, connection/storage settings, mobile bottom actions, 200% zoom behavior, reduced motion, modal focus management, and keyboard-only flows.

- [ ] **Step 4: Run Phase 4 verification**

Run:

```powershell
npm --prefix web run lint
npm --prefix web run typecheck
npm --prefix web run test
npm --prefix web run build
npm --prefix web run test:e2e
```

Expected: PASS in Chromium, Firefox, WebKit, desktop, and mobile projects.

- [ ] **Step 5: Commit**

```powershell
git add web/src web/e2e
git commit -m "feat: complete responsive Cellar UI"
```
