//! Process spawn: child fork, pipes, reader loops and the initialize handshake.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::time::Instant;

use parking_lot::Mutex as SyncMutex;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::watch;

use crate::diagnostics::DiagnosticsCache;
use crate::error::LspError;
use crate::rpc::FrameReader;
use crate::uri::path_to_uri;

use super::io::{read_loop, stderr_loop};
use super::types::SpawnRequest;
use super::{Inner, LspClient};

impl LspClient {
    /// Spawn the child and complete the `initialize` handshake.
    pub async fn spawn(request: SpawnRequest) -> Result<Self, LspError> {
        let SpawnRequest {
            server,
            root,
            argv,
            events,
            init_timeout,
            request_timeout,
            diagnostics_wait,
        } = request;
        let Some(program) = argv.first() else {
            return Err(LspError::Unavailable {
                server: server.id,
                reason: "no command configured".into(),
            });
        };

        let mut command = Command::new(program);
        command
            .args(argv.iter().skip(1))
            .current_dir(&root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Tokio kills the direct child on drop; unsafe Job Object code is
            // forbidden by the workspace lints.
            .kill_on_drop(true);
        if !server.env.is_empty() {
            command.envs(server.env.clone());
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|error| LspError::Spawn {
            server: server.id.clone(),
            message: error.to_string(),
            missing: error.kind() == std::io::ErrorKind::NotFound,
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pid = child.id();

        let (diag_seq, _) = watch::channel(0_u64);
        let root_uri = path_to_uri(&root);
        let settings = server.initialization.clone().unwrap_or(Value::Null);
        let inner = Arc::new(Inner {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            root_uri,
            root: root.clone(),
            stdin: AsyncMutex::new(stdin),
            child: AsyncMutex::new(Some(child)),
            next_id: AtomicI64::new(1),
            pending: SyncMutex::new(HashMap::new()),
            diagnostics: SyncMutex::new(DiagnosticsCache::default()),
            open_files: SyncMutex::new(HashMap::new()),
            settings,
            capabilities: SyncMutex::new(Value::Null),
            pull_supported: AtomicBool::new(false),
            events,
            in_flight: AtomicUsize::new(0),
            last_used: SyncMutex::new(Instant::now()),
            exited: AtomicBool::new(false),
            shutdown_started: AtomicBool::new(false),
            diag_seq,
            debounce_pending: SyncMutex::new(HashSet::new()),
            debouncing: AtomicBool::new(false),
            stderr: SyncMutex::new(String::new()),
            request_timeout,
            diagnostics_wait,
            pid,
        });

        if let Some(stdout) = stdout {
            let reader = FrameReader::new(BufReader::new(stdout));
            tokio::spawn(read_loop(Arc::clone(&inner), reader));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(stderr_loop(Arc::clone(&inner), stderr));
        }

        let client = Self { inner };
        match client.initialize(&server, init_timeout).await {
            Ok(()) => Ok(client),
            Err(error) => {
                client.force_kill().await;
                Err(error)
            }
        }
    }
}
