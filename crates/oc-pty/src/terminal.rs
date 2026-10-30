//! One terminal: status, bounded output, child control and background threads.

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use portable_pty::{Child, ChildKiller, MasterPty, PtySize};

use crate::buffer::RingBuffer;
use crate::manager::Shared;
use crate::{PtyError, PtyEvent, PtyInfo, PtyOutput, TERMINATE_GRACE};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy)]
struct Status {
    running: bool,
    exit: Option<i32>,
}

pub(crate) struct Terminal {
    id: String,
    created_ms: i64,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    location: Option<PathBuf>,
    pid: Option<u32>,
    title: Mutex<Option<String>>,
    /// Set when a caller supplied or changed the title; OSC reports must not
    /// overwrite an explicit title.
    title_locked: AtomicBool,
    size: Mutex<(u16, u16)>,
    status: Mutex<Status>,
    status_changed: Condvar,
    buffer: Mutex<RingBuffer>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    terminating: AtomicBool,
    shared: Weak<Shared>,
}

impl fmt::Debug for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = *lock(&self.status);
        f.debug_struct("Terminal")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .field("running", &status.running)
            .field("exit", &status.exit)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    #[allow(clippy::too_many_arguments)] // flat constructor mirrors the wire info
    pub(crate) fn new(
        id: String,
        created_ms: i64,
        command: String,
        args: Vec<String>,
        cwd: PathBuf,
        location: Option<PathBuf>,
        pid: Option<u32>,
        size: (u16, u16),
        title: Option<String>,
        buffer_bytes: usize,
        shared: Weak<Shared>,
    ) -> Self {
        let title_locked = title.is_some();
        Self {
            id,
            created_ms,
            command,
            args,
            cwd,
            location,
            pid,
            title: Mutex::new(title),
            title_locked: AtomicBool::new(title_locked),
            size: Mutex::new(size),
            status: Mutex::new(Status {
                running: true,
                exit: None,
            }),
            status_changed: Condvar::new(),
            buffer: Mutex::new(RingBuffer::new(buffer_bytes)),
            master: Mutex::new(None),
            writer: Mutex::new(None),
            killer: Mutex::new(None),
            threads: Mutex::new(Vec::new()),
            terminating: AtomicBool::new(false),
            shared,
        }
    }

    pub(crate) fn attach_io(
        &self,
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        killer: Box<dyn ChildKiller + Send + Sync>,
    ) {
        *lock(&self.master) = Some(master);
        *lock(&self.writer) = Some(writer);
        *lock(&self.killer) = Some(killer);
    }

    pub(crate) fn info(&self) -> PtyInfo {
        let status = *lock(&self.status);
        let (cols, rows) = *lock(&self.size);
        let buffer = lock(&self.buffer);
        PtyInfo {
            id: self.id.clone(),
            title: lock(&self.title).clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            location: self.location.clone(),
            pid: self.pid,
            cols,
            rows,
            running: status.running,
            exit: status.exit,
            created_ms: self.created_ms,
            output_head: buffer.start_byte(),
            output_bytes: buffer.end_byte(),
        }
    }

    /// Replaces the title with a caller-supplied value (including empty) and
    /// publishes [`PtyEvent::Title`] when it changed. Explicit titles are not
    /// overwritten by later OSC reports.
    pub(crate) fn rename(&self, title: String) {
        self.title_locked.store(true, Ordering::SeqCst);
        let changed = {
            let mut current = lock(&self.title);
            if current.as_deref() == Some(title.as_str()) {
                false
            } else {
                *current = Some(title.clone());
                true
            }
        };
        if changed {
            self.emit(PtyEvent::Title {
                id: self.id.clone(),
                title,
            });
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        lock(&self.status).running
    }

    pub(crate) fn write(&self, data: &[u8]) -> Result<(), PtyError> {
        if !self.is_running() {
            return Err(PtyError::Exited {
                id: self.id.clone(),
            });
        }
        let mut guard = lock(&self.writer);
        let Some(writer) = guard.as_mut() else {
            return Err(PtyError::Exited {
                id: self.id.clone(),
            });
        };
        writer
            .write_all(data)
            .map_err(|error| PtyError::Operation {
                operation: "write",
                message: error.to_string(),
            })?;
        writer.flush().map_err(|error| PtyError::Operation {
            operation: "flush",
            message: error.to_string(),
        })
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        if !self.is_running() {
            return Err(PtyError::Exited {
                id: self.id.clone(),
            });
        }
        let master = lock(&self.master);
        {
            let Some(master) = master.as_ref() else {
                return Err(PtyError::Exited {
                    id: self.id.clone(),
                });
            };
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| PtyError::Operation {
                    operation: "resize",
                    message: error.to_string(),
                })?;
        }
        drop(master);
        *lock(&self.size) = (cols, rows);
        Ok(())
    }

    pub(crate) fn snapshot(&self, rows: usize) -> String {
        lock(&self.buffer).snapshot(rows)
    }

    pub(crate) fn rows(&self, start: u64, count: usize) -> Result<Vec<String>, PtyError> {
        let buffer = lock(&self.buffer);
        let oldest = buffer.base_row();
        if start < oldest {
            return Err(PtyError::RowGap {
                requested: start,
                oldest,
            });
        }
        Ok(buffer.rows(start, count))
    }

    pub(crate) fn replay(&self, cursor: Option<u64>) -> PtyOutput {
        let (bytes, cursor) = lock(&self.buffer).replay(cursor);
        PtyOutput { bytes, cursor }
    }

    fn emit(&self, event: PtyEvent) {
        if let Some(shared) = self.shared.upgrade() {
            let _ = shared.events.send(event);
        }
    }

    fn set_title(&self, title: String) {
        if title.is_empty() || self.title_locked.load(Ordering::SeqCst) {
            return;
        }
        let changed = {
            let mut current = lock(&self.title);
            if current.as_deref() == Some(title.as_str()) {
                false
            } else {
                *current = Some(title.clone());
                true
            }
        };
        if changed {
            self.emit(PtyEvent::Title {
                id: self.id.clone(),
                title,
            });
        }
    }

    /// Marks the terminal exited exactly once and publishes [`PtyEvent::Exited`].
    pub(crate) fn mark_exited(&self, exit: Option<i32>) -> bool {
        {
            let mut status = lock(&self.status);
            if !status.running {
                return false;
            }
            status.running = false;
            status.exit = exit;
        }
        self.status_changed.notify_all();
        self.emit(PtyEvent::Exited {
            id: self.id.clone(),
            exit,
        });
        if let Some(shared) = self.shared.upgrade() {
            shared.retire(&self.id);
        }
        true
    }

    pub(crate) fn wait_exited(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut status = lock(&self.status);
        while status.running {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, outcome) = self
                .status_changed
                .wait_timeout(status, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            status = guard;
            if outcome.timed_out() && status.running {
                return false;
            }
        }
        true
    }

    /// Sends the kill signal and waits asynchronously (bounded by
    /// [`TERMINATE_GRACE`]) for the exit watcher, force-marking on timeout.
    pub(crate) fn request_terminate(self: &Arc<Self>) {
        if !self.is_running() || self.terminating.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut killer = lock(&self.killer);
            if let Some(killer) = killer.as_mut() {
                let _ = killer.kill();
            }
        }
        let terminal = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name(format!("oc-pty-terminate-{}", self.id))
            .spawn(move || {
                if !terminal.wait_exited(TERMINATE_GRACE) {
                    terminal.mark_exited(None);
                }
            });
        if let Ok(handle) = spawned {
            lock(&self.threads).push(handle);
        }
    }

    /// Last-resort signal used by `Drop`, without spawning a watcher.
    pub(crate) fn kill_now(&self) {
        let mut killer = lock(&self.killer);
        if let Some(killer) = killer.as_mut() {
            let _ = killer.kill();
        }
    }

    pub(crate) fn start_watcher(
        self: &Arc<Self>,
        mut child: Box<dyn Child + Send + Sync>,
    ) -> std::io::Result<()> {
        let terminal = Arc::clone(self);
        let handle = thread::Builder::new()
            .name(format!("oc-pty-wait-{}", self.id))
            .spawn(move || {
                let exit = child
                    .wait()
                    .ok()
                    .map(|status| i32::try_from(status.exit_code()).unwrap_or(-1));
                drop(child);
                terminal.mark_exited(exit);
            })?;
        lock(&self.threads).push(handle);
        Ok(())
    }

    pub(crate) fn start_reader(
        self: &Arc<Self>,
        mut reader: Box<dyn Read + Send>,
    ) -> std::io::Result<()> {
        let terminal = Arc::clone(self);
        let handle = thread::Builder::new()
            .name(format!("oc-pty-read-{}", self.id))
            .spawn(move || {
                let mut chunk = [0_u8; 8192];
                let mut titles = TitleScanner::default();
                #[cfg(windows)]
                let mut startup = ConPtyStartup::default();
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => {
                            let bytes = &chunk[..count];
                            #[cfg(windows)]
                            let (output, answer_cursor) = startup.process(bytes);
                            #[cfg(windows)]
                            let bytes: &[u8] = output.as_deref().unwrap_or(bytes);
                            lock(&terminal.buffer).append(bytes);
                            terminal.emit(PtyEvent::Output {
                                id: terminal.id.clone(),
                                bytes: Bytes::copy_from_slice(bytes),
                            });
                            #[cfg(windows)]
                            if answer_cursor {
                                // ConPTY with INHERIT_CURSOR blocks the child
                                // until the attached terminal reports a cursor
                                // position; we are the attached terminal here.
                                let _ = terminal.write(b"\x1b[1;1R");
                            }
                            for title in titles.feed(bytes) {
                                terminal.set_title(title);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })?;
        lock(&self.threads).push(handle);
        Ok(())
    }

    /// Drops OS handles (EOF for the child) and joins helper threads with a
    /// bound so a stuck reader cannot hang the caller.
    pub(crate) fn close(&self, timeout: Duration) {
        lock(&self.killer).take();
        lock(&self.writer).take();
        lock(&self.master).take();
        let handles = std::mem::take(&mut *lock(&self.threads));
        join_bounded(handles, timeout);
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.kill_now();
        if let Ok(writer) = self.writer.get_mut() {
            writer.take();
        }
        if let Ok(master) = self.master.get_mut() {
            master.take();
        }
    }
}

fn join_bounded(handles: Vec<JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    for handle in handles {
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TitleState {
    #[default]
    Ground,
    Esc,
    Csi,
    OscParams,
    OscText,
    OscEsc,
}

/// Extracts xterm title reports (OSC 0/1/2) across chunk boundaries.
///
/// The scanner is per-terminal and sees every raw chunk; it does not affect
/// the bytes retained for snapshots.
#[derive(Debug, Default)]
struct TitleScanner {
    state: TitleState,
    params: Vec<u8>,
    text: Vec<u8>,
    overflow: bool,
}

impl TitleScanner {
    fn feed(&mut self, input: &[u8]) -> Vec<String> {
        let mut titles = Vec::new();
        for &byte in input {
            match self.state {
                TitleState::Ground => {
                    if byte == 0x1B {
                        self.state = TitleState::Esc;
                    }
                }
                TitleState::Esc => match byte {
                    b']' => {
                        self.params.clear();
                        self.text.clear();
                        self.overflow = false;
                        self.state = TitleState::OscParams;
                    }
                    b'[' => self.state = TitleState::Csi,
                    0x1B => {}
                    _ => self.state = TitleState::Ground,
                },
                TitleState::Csi => {
                    if (0x40..=0x7E).contains(&byte) {
                        self.state = TitleState::Ground;
                    } else if byte == 0x1B {
                        self.state = TitleState::Esc;
                    }
                }
                TitleState::OscParams => match byte {
                    b';' => self.state = TitleState::OscText,
                    0x07 => self.state = TitleState::Ground,
                    0x1B => self.state = TitleState::OscEsc,
                    _ if self.params.len() < 8 => self.params.push(byte),
                    _ => {}
                },
                TitleState::OscText => match byte {
                    0x07 => {
                        if let Some(title) = self.finish() {
                            titles.push(title);
                        }
                        self.state = TitleState::Ground;
                    }
                    0x1B => self.state = TitleState::OscEsc,
                    _ => self.push_text(byte),
                },
                TitleState::OscEsc => {
                    if byte == b'\\' {
                        if let Some(title) = self.finish() {
                            titles.push(title);
                        }
                        self.state = TitleState::Ground;
                    } else {
                        self.state = TitleState::OscText;
                        self.push_text(byte);
                    }
                }
            }
        }
        titles
    }

    fn push_text(&mut self, byte: u8) {
        if self.overflow {
            return;
        }
        if self.text.len() < 256 {
            self.text.push(byte);
        } else {
            self.overflow = true;
        }
    }

    fn finish(&mut self) -> Option<String> {
        let params = std::mem::take(&mut self.params);
        let text = std::mem::take(&mut self.text);
        self.overflow = false;
        if !matches!(params.as_slice(), b"0" | b"1" | b"2") {
            return None;
        }
        let title = String::from_utf8_lossy(&text).trim().to_string();
        (!title.is_empty()).then_some(title)
    }
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct ConPtyStartup {
    carry: Vec<u8>,
    done: bool,
}

/// `ConPTY` (portable-pty uses `PSEUDOCONSOLE_INHERIT_CURSOR`) emits `ESC[6n`
/// before the child starts and withholds output until the attached terminal
/// answers with a cursor position. We are the attached terminal, so answer
/// once with the top-left position and keep the query out of the output stream;
/// later queries belong to the real WS client.
#[cfg(windows)]
impl ConPtyStartup {
    /// `None` means "pass the chunk through untouched".
    fn process(&mut self, chunk: &[u8]) -> (Option<Vec<u8>>, bool) {
        if self.done {
            return (None, false);
        }
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(chunk);
        let initialized = data.windows(7).any(|window| window == b"\x1b[?9001");
        let mut output = Vec::with_capacity(data.len());
        let mut answered = false;
        let mut index = 0;
        while index < data.len() {
            if data[index] == 0x1b {
                let rest = &data[index..];
                if rest.starts_with(b"\x1b[6n") {
                    answered = true;
                    index += 4;
                    continue;
                }
                if rest.len() < 4 && b"\x1b[6n".starts_with(rest) {
                    break;
                }
            }
            output.push(data[index]);
            index += 1;
        }
        self.carry = data[index..].to_vec();
        if initialized || answered {
            self.done = true;
            output.extend_from_slice(&self.carry);
            self.carry.clear();
        }
        (Some(output), answered)
    }
}

#[cfg(test)]
mod tests {
    use super::TitleScanner;

    #[test]
    fn scans_titles_across_chunks() {
        let mut scanner = TitleScanner::default();
        assert!(scanner.feed(b"\x1b]2;my ").is_empty());
        assert_eq!(scanner.feed(b"title\x07"), vec!["my title"]);
        // Non-title OSC is ignored.
        assert!(scanner.feed(b"\x1b]8;;http://x\x07").is_empty());
        // BEL and ST terminators both work.
        assert_eq!(scanner.feed(b"\x1b]0;a\x1b\\"), vec!["a"]);
    }

    #[cfg(windows)]
    #[test]
    fn answers_and_filters_the_conpty_cursor_query() {
        use super::ConPtyStartup;

        let mut startup = ConPtyStartup::default();
        assert_eq!(
            startup.process(b"\x1b[6n\x1b[?9001h"),
            (Some(b"\x1b[?9001h".to_vec()), true)
        );
        // Already initialised: later queries are for the real client.
        assert_eq!(startup.process(b"\x1b[6n"), (None, false));

        let mut split = ConPtyStartup::default();
        assert_eq!(split.process(b"\x1b[6"), (Some(Vec::new()), false));
        assert_eq!(split.process(b"nok"), (Some(b"ok".to_vec()), true));
    }
}
