//! `todowrite` tool: validates and stores the session's todo list.
//!
//! Every call replaces the previous list for the calling session. Lists live
//! in bounded process memory (at most [`MAX_SESSIONS`] sessions, oldest
//! evicted first); they are never written to disk. The rendered checklist is
//! returned as the tool output and the structured list as metadata.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex, PoisonError};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, ToolResult};

/// Maximum number of sessions whose todo lists are retained.
pub const MAX_SESSIONS: usize = 100;

/// Allowed todo statuses, in display order.
pub const STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

/// One todo item as supplied by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Default)]
struct TodoStore {
    /// Session ids in last-write order; the front is evicted first.
    order: VecDeque<String>,
    by_session: HashMap<String, Vec<Todo>>,
}

impl TodoStore {
    fn replace(&mut self, session: &str, todos: Vec<Todo>) {
        if self.by_session.contains_key(session) {
            self.order.retain(|id| id != session);
        } else {
            while self.order.len() >= MAX_SESSIONS {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.by_session.remove(&oldest);
            }
        }
        self.order.push_back(session.to_string());
        self.by_session.insert(session.to_string(), todos);
    }
}

static TODOS: LazyLock<Mutex<TodoStore>> = LazyLock::new(|| Mutex::new(TodoStore::default()));

fn store(session: &str, todos: Vec<Todo>) {
    let mut state = TODOS.lock().unwrap_or_else(PoisonError::into_inner);
    state.replace(session, todos);
}

/// `todowrite` tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct TodowriteTool;

#[async_trait]
impl Tool for TodowriteTool {
    fn name(&self) -> &str {
        "todowrite"
    }

    fn description(&self) -> &str {
        "Create and update a todo list for the current session. The user sees it as your working plan. Use it proactively when a task has 3 or more distinct steps, or when the user gives several tasks or asks for a todo list. Skip it for a single simple task or a purely conversational request. When in doubt, use it. Send the full list on every call; it replaces the previous one. Mark items `in_progress` when you start them and `completed` as soon as they are actually done. Only call it when the list changes."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list. Always send the complete list; it replaces the previous one.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Brief description of the task"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": "Current status of the task: pending, in_progress, completed, cancelled"
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let todos = parse_todos(&input)?;
        let output = render(&todos);
        let structured = serde_json::to_value(&todos)
            .map_err(|error| ToolError::new(format!("Unable to serialise todos: {error}")))?;
        store(&ctx.session_id, todos);
        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({ "todos": structured })),
        ))
    }
}

fn parse_todos(input: &Value) -> Result<Vec<Todo>, ToolError> {
    let value = input
        .get("todos")
        .ok_or_else(|| ToolError::new("Invalid arguments: missing required argument `todos`"))?;
    let Value::Array(items) = value else {
        return Err(ToolError::new(
            "Invalid arguments: `todos` must be an array",
        ));
    };
    let mut todos = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let object = item.as_object().ok_or_else(|| {
            ToolError::new(format!(
                "Invalid arguments: `todos[{index}]` must be an object"
            ))
        })?;
        let content = match object.get("content") {
            Some(Value::String(content)) if !content.trim().is_empty() => {
                content.trim().to_string()
            }
            Some(Value::String(_)) => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: `todos[{index}].content` must not be empty"
                )));
            }
            Some(_) => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: `todos[{index}].content` must be a string"
                )));
            }
            None => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: missing required argument `todos[{index}].content`"
                )));
            }
        };
        let status = match object.get("status") {
            Some(Value::String(status)) if STATUSES.contains(&status.as_str()) => status.clone(),
            Some(Value::String(status)) => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: `todos[{index}].status` must be one of pending, in_progress, completed, cancelled (got `{status}`)"
                )));
            }
            Some(_) => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: `todos[{index}].status` must be a string"
                )));
            }
            None => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: missing required argument `todos[{index}].status`"
                )));
            }
        };
        todos.push(Todo { content, status });
    }
    Ok(todos)
}

fn marker(status: &str) -> &'static str {
    match status {
        "pending" => "- [ ] ",
        "in_progress" => "- [~] ",
        "completed" => "- [x] ",
        _ => "- [-] ",
    }
}

fn render(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Todos cleared".to_string();
    }
    let mut output = String::new();
    for todo in todos {
        output.push_str(marker(&todo.status));
        output.push_str(&todo.content);
        output.push('\n');
    }

    let count = |status: &str| todos.iter().filter(|todo| todo.status == status).count();
    let mut summary = format!(
        "Todos updated: {}/{} completed",
        count("completed"),
        todos.len()
    );
    for (status, label) in [
        ("in_progress", "in progress"),
        ("pending", "pending"),
        ("cancelled", "cancelled"),
    ] {
        let total = count(status);
        if total > 0 {
            let _ = write!(summary, ", {total} {label}");
        }
    }
    let active: Vec<&str> = todos
        .iter()
        .filter(|todo| todo.status == "in_progress")
        .map(|todo| todo.content.as_str())
        .collect();
    if !active.is_empty() {
        let _ = write!(summary, ". In progress: {}", active.join(", "));
    }
    output.push('\n');
    output.push_str(&summary);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_store_stays_bounded() {
        let mut state = TodoStore::default();
        for index in 0..(MAX_SESSIONS + 5) {
            state.replace(&format!("bounds_session_{index}"), Vec::new());
        }
        assert_eq!(state.order.len(), MAX_SESSIONS);
        assert_eq!(state.order.len(), state.by_session.len());
        assert!(!state.by_session.contains_key("bounds_session_0"));
        assert!(
            state
                .by_session
                .contains_key(&format!("bounds_session_{}", MAX_SESSIONS + 4))
        );

        // Replacing an existing session must not grow the order list.
        state.replace("bounds_replace", Vec::new());
        state.replace("bounds_replace", Vec::new());
        assert_eq!(
            state
                .order
                .iter()
                .filter(|id| id.as_str() == "bounds_replace")
                .count(),
            1
        );
        assert!(state.order.len() <= MAX_SESSIONS);
    }

    #[test]
    fn sessions_keep_separate_lists() {
        let first = vec![Todo {
            content: "first".to_string(),
            status: "pending".to_string(),
        }];
        let second = vec![Todo {
            content: "second".to_string(),
            status: "completed".to_string(),
        }];
        let mut state = TodoStore::default();
        state.replace("isolation_session_a", first.clone());
        state.replace("isolation_session_b", second.clone());
        assert_eq!(state.by_session.get("isolation_session_a"), Some(&first));
        assert_eq!(state.by_session.get("isolation_session_b"), Some(&second));
    }

    #[test]
    fn render_marks_every_status() {
        let todos = vec![
            Todo {
                content: "a".to_string(),
                status: "pending".to_string(),
            },
            Todo {
                content: "b".to_string(),
                status: "in_progress".to_string(),
            },
            Todo {
                content: "c".to_string(),
                status: "completed".to_string(),
            },
            Todo {
                content: "d".to_string(),
                status: "cancelled".to_string(),
            },
        ];
        let output = render(&todos);
        assert!(output.contains("- [ ] a"), "{output}");
        assert!(output.contains("- [~] b"), "{output}");
        assert!(output.contains("- [x] c"), "{output}");
        assert!(output.contains("- [-] d"), "{output}");
        assert!(output.contains("1/4 completed"), "{output}");
        assert!(output.contains("In progress: b"), "{output}");
        assert_eq!(render(&[]), "Todos cleared");
    }
}
