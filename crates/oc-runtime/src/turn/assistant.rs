use oc_types::message::{
    AssistantPart, FinishReason, ReasoningPart, StructuredError, TextPart, ToolPart, ToolState,
    ToolTime,
};
use oc_types::{MessageId, ModelRef, Tokens};
use serde_json::{Map, Value};

/// Mutable assistant message for one step.
#[derive(Debug, Clone)]
pub(super) struct AssistantState {
    pub(super) id: MessageId,
    pub(super) agent: String,
    pub(super) model: ModelRef,
    pub(super) created: i64,
    pub(super) content: Vec<AssistantPart>,
    pub(super) streamed: Option<i64>,
    pub(super) completed: Option<i64>,
    pub(super) finish: Option<FinishReason>,
    pub(super) raw_finish: Option<String>,
    pub(super) tokens: Option<Tokens>,
    pub(super) cost: Option<f64>,
    pub(super) error: Option<StructuredError>,
    pub(super) open_text: Option<usize>,
    pub(super) open_reasoning: Option<usize>,
}

impl AssistantState {
    pub(super) fn new(id: MessageId, agent: String, model: ModelRef, created: i64) -> Self {
        Self {
            id,
            agent,
            model,
            created,
            content: Vec::new(),
            streamed: None,
            completed: None,
            finish: None,
            raw_finish: None,
            tokens: None,
            cost: None,
            error: None,
            open_text: None,
            open_reasoning: None,
        }
    }

    pub(super) fn data(&self) -> Value {
        let mut time = Map::new();
        time.insert("created".to_owned(), Value::from(self.created));
        if let Some(streamed) = self.streamed {
            time.insert("streamed".to_owned(), Value::from(streamed));
        }
        if let Some(completed) = self.completed {
            time.insert("completed".to_owned(), Value::from(completed));
        }
        let mut map = Map::new();
        map.insert("time".to_owned(), Value::Object(time));
        map.insert("agent".to_owned(), Value::String(self.agent.clone()));
        map.insert(
            "model".to_owned(),
            serde_json::to_value(&self.model).expect("model ref is serializable"),
        );
        map.insert(
            "content".to_owned(),
            serde_json::to_value(&self.content).expect("assistant content is serializable"),
        );
        if let Some(finish) = self.finish {
            map.insert(
                "finish".to_owned(),
                serde_json::to_value(finish).expect("finish reason is serializable"),
            );
        }
        if let Some(raw) = &self.raw_finish {
            map.insert("rawFinish".to_owned(), Value::String(raw.clone()));
        }
        if let Some(tokens) = &self.tokens {
            map.insert(
                "tokens".to_owned(),
                serde_json::to_value(tokens).expect("tokens are serializable"),
            );
        }
        if let Some(cost) = self.cost {
            map.insert("cost".to_owned(), Value::from(cost));
        }
        if let Some(error) = &self.error {
            map.insert(
                "error".to_owned(),
                serde_json::to_value(error).expect("structured error is serializable"),
            );
        }
        Value::Object(map)
    }

    pub(super) fn ensure_text(&mut self) -> (usize, bool) {
        if let Some(index) = self.open_text {
            return (index, false);
        }
        let index = self.content.len();
        self.content.push(AssistantPart::Text(TextPart {
            text: String::new(),
            native: None,
        }));
        self.open_text = Some(index);
        (index, true)
    }

    pub(super) fn ensure_reasoning(&mut self) -> (usize, bool) {
        if let Some(index) = self.open_reasoning {
            return (index, false);
        }
        let index = self.content.len();
        self.content.push(AssistantPart::Reasoning(ReasoningPart {
            text: String::new(),
            native: None,
            time: None,
        }));
        self.open_reasoning = Some(index);
        (index, true)
    }

    pub(super) fn append_text(&mut self, delta: &str) {
        let (index, _) = self.ensure_text();
        if let Some(AssistantPart::Text(part)) = self.content.get_mut(index) {
            part.text.push_str(delta);
        }
    }

    pub(super) fn append_reasoning(&mut self, delta: &str) {
        let (index, _) = self.ensure_reasoning();
        if let Some(AssistantPart::Reasoning(part)) = self.content.get_mut(index) {
            part.text.push_str(delta);
        }
    }

    pub(super) fn close_text(&mut self) -> Option<(usize, String)> {
        let index = self.open_text.take()?;
        let text = match self.content.get(index) {
            Some(AssistantPart::Text(part)) => part.text.clone(),
            _ => String::new(),
        };
        Some((index, text))
    }

    pub(super) fn close_reasoning(&mut self) -> Option<(usize, String)> {
        let index = self.open_reasoning.take()?;
        let text = match self.content.get(index) {
            Some(AssistantPart::Reasoning(part)) => part.text.clone(),
            _ => String::new(),
        };
        if let Some(AssistantPart::Reasoning(part)) = self.content.get_mut(index) {
            part.time = Some(oc_types::message::CompletedTime {
                created: self.created,
                completed: Some(oc_types::now_millis()),
            });
        }
        Some((index, text))
    }

    pub(super) fn tool_index(&self, id: &str) -> Option<usize> {
        self.content
            .iter()
            .position(|part| matches!(part, AssistantPart::Tool(tool) if tool.id == id))
    }

    pub(super) fn ensure_tool(&mut self, id: &str, name: &str) -> usize {
        if let Some(index) = self.tool_index(id) {
            return index;
        }
        let index = self.content.len();
        self.content.push(AssistantPart::Tool(Box::new(ToolPart {
            id: id.to_owned(),
            name: name.to_owned(),
            executed: Some(false),
            native: None,
            state: ToolState::Streaming {
                input: String::new(),
            },
            time: ToolTime {
                created: oc_types::now_millis(),
                ran: None,
                completed: None,
            },
        })));
        index
    }

    pub(super) fn append_tool_input(&mut self, id: &str, delta: &str) {
        let index = self.ensure_tool(id, "");
        if let Some(AssistantPart::Tool(tool)) = self.content.get_mut(index)
            && let ToolState::Streaming { input } = &mut tool.state
        {
            input.push_str(delta);
        }
    }

    pub(super) fn set_tool_streamed(&mut self, id: &str, text: String) {
        let index = self.ensure_tool(id, "");
        if let Some(AssistantPart::Tool(tool)) = self.content.get_mut(index)
            && let ToolState::Streaming { input } = &mut tool.state
        {
            *input = text;
        }
    }
}

pub(super) fn tool_id(state: &AssistantState, index: usize) -> String {
    match state.content.get(index) {
        Some(AssistantPart::Tool(tool)) => tool.id.clone(),
        _ => String::new(),
    }
}

pub(super) fn ordinal_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

pub(super) fn assistant_text(state: &AssistantState) -> String {
    state
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) if !text.text.trim().is_empty() => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
