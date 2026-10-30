//! Shared fixtures for the extended store tests.

use oc_store::{NewMessage, NewSchedule, NewSession, ScheduleAction, ScheduleWhen, Store};
use serde_json::{Value, json};

pub(crate) fn new_session(id: &str) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: None,
        project_id: "prj_1".to_owned(),
        directory: "/tmp/one".to_owned(),
        title: Some(format!("title {id}")),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        time_created: 1_000,
    }
}

pub(crate) fn new_message(session: &str, id: &str, created: i64) -> NewMessage {
    NewMessage {
        id: id.to_owned(),
        session_id: session.to_owned(),
        kind: "user".to_owned(),
        data: json!({ "text": id }),
        time_created: created,
    }
}

pub(crate) fn ids(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .map(|value| value["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

pub(crate) async fn seed_session(store: &Store, id: &str) {
    store
        .create_session(new_session(id))
        .await
        .expect("create session");
}

pub(crate) fn schedule_prompt(prompt: &str) -> ScheduleAction {
    ScheduleAction::Prompt {
        prompt: prompt.to_owned(),
    }
}

pub(crate) fn scheduled_in(
    session: &str,
    name: Option<&str>,
    duration: &str,
    prompt: &str,
    created: i64,
) -> NewSchedule {
    NewSchedule {
        session_id: session.to_owned(),
        name: name.map(str::to_owned),
        when: ScheduleWhen::In {
            r#in: duration.to_owned(),
            tz: None,
        },
        ends: None,
        action: schedule_prompt(prompt),
        time_created: created,
    }
}

pub(crate) fn cron_when(expression: &str, tz: Option<&str>) -> ScheduleWhen {
    ScheduleWhen::Cron {
        cron: expression.to_owned(),
        tz: tz.map(str::to_owned),
    }
}
