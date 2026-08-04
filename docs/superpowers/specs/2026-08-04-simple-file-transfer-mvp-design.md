# Cellar Simple File Transfer MVP Design

## Goal

Cellar provides remote file transfer for this Windows PC through a small web app. The MVP keeps project-based organization and implements only file upload, listing, and download.

## Scope

The MVP includes:

- Create and list projects.
- Upload a file into a project with one HTTP request.
- List completed files in a project.
- Download a completed file.
- Store metadata in SQLite and file contents on the local disk.
- Protect remote access through the existing Cloudflare Access boundary.

The MVP excludes:

- Upload cancellation, resumption, recovery sessions, and chunk coordination.
- Persistent failed-upload records.
- Windows filesystem watching and synchronization of changes made through Explorer.
- File rename, move, delete, tagging, search, sharing links, and previews.

## Upload Flow

The server writes each upload to an application-owned temporary file in the target project's storage area. A completed request is validated and then published to its final path without overwriting an existing file. Only after publication succeeds is the file recorded as complete and returned by the file-list API.

If the request is interrupted or validation, storage, or database work fails, the server removes the temporary file. No resumable upload session remains. If the process terminates before cleanup, startup cleanup removes application-owned temporary upload files before serving requests.

## Download and Listing

The list endpoint returns only files that completed publication. The download endpoint resolves files by stored identity, keeps paths inside the configured storage root, and supports the existing authenticated remote boundary. Partial temporary files are never listed or downloadable.

## User Interface

The existing simple, brandless web app remains. A user selects a project, chooses a file, uploads it, sees progress for the current request, and downloads completed files from the list. A failed upload shows a failure message and can be retried from the beginning. There are no cancel, resume, recover, or abandon controls.

## Failure Handling

- Duplicate final filename: reject without overwriting.
- Interrupted request: fail and clean the temporary file.
- Invalid filename or path: reject before publication.
- Disk or database failure: return an error, preserve any already completed file safely, and remove unpublished temporary data.
- Process crash: remove owned temporary upload files during the next startup.

## Verification

Automated tests cover successful upload/list/download, duplicate-name rejection, interrupted or invalid upload cleanup, startup cleanup, authentication and Origin enforcement, path containment, and the browser success/failure flows. A local acceptance test uploads and downloads a representative file and verifies its bytes.

## Deferred Work

Windows filesystem watching is a follow-up. If added, it will reconcile files changed outside Cellar without changing the MVP upload and download contract.
