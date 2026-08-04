# Cellar Upload Abandon Design

**Date:** 2026-08-04

## Purpose

Cellar must let the owner leave a resumable upload when its original local file is no longer available, without repeating the unsafe browser-only discard behavior that erased the sole recovery reference while the server session and staging bytes remained active.

The operation is intentionally narrow. It abandons one upload session and its exact staging file; it never deletes a completed project file, a project, or any unrelated storage entry.

## HTTP Contract

Add:

```text
DELETE /api/v1/uploads/{uploadId}
```

The route stays inside the existing API boundary:

```text
request ID -> tracing -> Cloudflare Access assertion -> exact unsafe-method Origin -> route
```

Responses use the existing error envelope and request ID contract:

- `204 No Content`: the active or failed session is durably abandoned and its staging entry is absent.
- `400 Bad Request`: the upload ID is not a canonical lower-case UUID.
- `401 Unauthorized` / `403 Forbidden`: existing Access and Origin rules.
- `404 Not Found`: the session does not exist. The browser may treat this as terminal evidence and forget its local recovery metadata.
- `409 Conflict`: the session is `finalizing` or `complete`; Cellar never deletes a published or potentially publishing file through this endpoint.
- `503 Service Unavailable`: database transition or exact staging cleanup is ambiguous. The browser retains recovery metadata and offers retry.

## Server State and Storage Rules

The abandon operation acquires the existing per-upload lock used by chunk upload and completion. This serializes it with an in-flight chunk or finalization operation.

For an `active` session:

1. transition the database session to `failed` with the closed reason `owner_abandoned`;
2. remove only `.cellar/uploads/<uploadId>.part` through the existing safe exact-entry storage primitive;
3. return `204` only after the exact staging entry is absent.

For a `failed` session, skip the state transition and retry the exact staging cleanup. This makes a prior partial abandon retryable. A missing staging entry is an idempotent success.

For `finalizing` and `complete`, return `409` without changing the database or filesystem. For an unknown session, return `404` without touching storage.

If the transition to `failed` is ambiguous, do not delete staging evidence. If the transition succeeds but staging cleanup fails, retain the `failed` row and return `503`; a later DELETE retries cleanup. Unsafe or non-regular exact staging entries are preserved and reported as a safe failure, never followed or recursively deleted.

## Browser Behavior

The upload panel exposes **Abandon upload** only when it has a recoverable server session and is not actively sending a request. The owner must confirm the destructive staging cleanup before the DELETE is sent.

The browser clears the exact five-field `localStorage` record and returns to the empty upload state only after:

- DELETE succeeds with `204`; or
- an authoritative status/DELETE response is `404`, proving the server session no longer exists.

An authoritative `failed` status is terminal but may still have staging evidence, so the panel offers **Remove failed upload**, which uses the same DELETE cleanup path before clearing metadata. Network errors, `503`, and `409` retain metadata and show a safe retryable message.

The UI never offers local-only discard for an active, paused, retryable, or recovered session. It continues to enforce one recoverable upload at a time.

## Tests

Server tests must prove:

- authentication and exact Origin are required;
- active abandon transitions to `failed`, deletes only the exact staging file, and returns `204`;
- abandon serializes with an in-flight chunk and cannot delete newly committed bytes out of order;
- failed-session retry removes remaining staging and is idempotent when staging is already absent;
- transition ambiguity preserves staging and returns `503`;
- unsafe cleanup preserves the entry and returns `503`;
- unknown returns `404`; `finalizing` and `complete` return `409` without mutation.

Client and component tests must prove:

- active/recovered sessions are forgotten only after confirmed DELETE success or authoritative `404`;
- failed sessions use server cleanup before local reset;
- cancellation of the DELETE, network failure, `409`, or `503` retains all five metadata fields;
- while abandon is pending, controls are disabled and no second upload can start;
- the finalizing live-reconciliation flow remains unchanged.

The full Rust, web, acceptance, lint, and release verification gates remain required.

## Explicit Non-Goals

- automatic age-based upload garbage collection;
- bulk cleanup or operator-wide purge;
- deleting completed files or projects;
- trash, restore, rename, move, or copy features;
- changing Cloudflare Tunnel or Access provisioning.
