//! `/api/shell` and the persistent-PTY aliases.
//!
//! [`ShellManager`] runs one non-interactive command per shell: combined
//! stdout/stderr are appended to a bounded in-process ring (1 MiB) while the
//! same bytes are captured in a temp file exposed as `Shell.Info.file`. Output
//! is paged by absolute byte cursor; evicted cursors are clamped to the oldest
//! retained byte and reported with `truncated: true`.
//!
//! The manager is process-global ([`manager`]); exited shells stay observable
//! until `EXITED_LIMIT` newer ones have exited or a client deletes them.
//!
//! # HTTP
//!
//! - `GET /api/shell` → running shells in a location envelope.
//! - `POST /api/shell` → `{command, cwd?, timeout?, metadata?}`, publishes
//!   `shell.created`.
//! - `GET /api/shell/:id` → one `Shell.Info` (running or exited).
//! - `DELETE /api/shell/:id` → kill the process tree when running, remove the
//!   temp file, publish `shell.exited` (when it was running) then
//!   `shell.deleted`, answer 204.
//! - `GET /api/shell/:id/output?cursor&limit` → `{output, cursor, size,
//!   truncated}`.
//! - `POST /api/session/:sid/shell` → start in the session directory with
//!   `OPENCODE_SESSION_ID` set, answer 204.
//! - `GET|POST /api/experimental/session/:sid/terminal`, `GET .../read` and the
//!   `/api/experimental/persistent-pty*` aliases expose [`oc_pty::PtyManager`]
//!   terminals that were created through them, bound to a session id.
//!
//! The persistent-PTY surface is an alias over `oc-pty`, which retains raw
//! bytes rather than a terminal cell model. `read`/`snapshot` therefore report
//! the retained text and an end-of-output cursor; the checkpoint is the raw
//! retained byte stream. No screen content is fabricated.
//! This module is the facade: it owns `routes()` and re-exports the manager surface.

use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};

use crate::AppState;

const SHELL_BUFFER: usize = 1024 * 1024;

/// Exited shells kept observable before the oldest is evicted.
const EXITED_LIMIT: usize = 16;

/// Upstream's default page size for `output`.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// How long `DELETE` waits for a killed child before removing it anyway.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// How long the exit watcher waits for the output reader to drain the file.
const DRAIN_WAIT: Duration = Duration::from_secs(5);

const SHELL_DIR: &str = "opencode-shell";
const TICKET_HEADER: &str = "x-opencode-ticket";

mod http;
mod manager;
mod persistent;
mod selection;
mod util;

pub(crate) use http::{create, output, session_shell};
pub use http::{get_one, list, remove};
pub use manager::{CreateShell, ShellManager, manager};
pub use persistent::{
    persistent_connect_token, persistent_get, persistent_remove, persistent_snapshot, terminal_list,
};
pub(crate) use persistent::{
    persistent_create, persistent_list, persistent_update, terminal_create, terminal_read,
};

/// Shell routes; nested by the router owner.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/shell", get(list).post(create))
        .route("/api/shell/{id}", get(get_one).delete(remove))
        .route("/api/shell/{id}/output", get(output))
        .route("/api/session/{sid}/shell", post(session_shell))
        .route(
            "/api/experimental/session/{sid}/terminal",
            get(terminal_list).post(terminal_create),
        )
        .route(
            "/api/experimental/session/{sid}/terminal/read",
            get(terminal_read),
        )
        .route(
            "/api/experimental/persistent-pty",
            get(persistent_list).post(persistent_create),
        )
        .route(
            "/api/experimental/persistent-pty/{id}",
            get(persistent_get)
                .put(persistent_update)
                .delete(persistent_remove),
        )
        .route(
            "/api/experimental/persistent-pty/{id}/snapshot",
            get(persistent_snapshot),
        )
        .route(
            "/api/experimental/persistent-pty/{id}/connect-token",
            post(persistent_connect_token),
        )
}
