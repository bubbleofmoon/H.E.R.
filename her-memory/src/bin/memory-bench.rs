use std::error::Error;
use std::io::{self, Read, Write};

use her_memory::{prepare_connection, recall, MemoryStore, NewMemory};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Turn {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct RecallInput {
    question: String,
    haystack_sessions: Vec<Vec<Turn>>,
}

#[derive(Deserialize)]
struct Question {
    question_id: String,
    answer: String,
    #[serde(flatten)]
    input: RecallInput,
}

/// A benchmark implementation ingests one question's history and answers it in isolation.
/// The reference answer and evidence labels are outside this interface.
trait RecallAdapter {
    fn answer(&self, input: &RecallInput) -> Result<Option<String>, Box<dyn Error>>;
}

struct CurrentMemory;

impl RecallAdapter for CurrentMemory {
    fn answer(&self, input: &RecallInput) -> Result<Option<String>, Box<dyn Error>> {
        let connection = prepare_connection(Connection::open_in_memory()?)?;
        let store = MemoryStore::from_connection(connection, None);
        let reader = store.for_reader("private:benchmark");
        for turn in input.haystack_sessions.iter().flatten() {
            reader.write(&NewMemory {
                owner_type: "session",
                scope: None,
                kind: "fact",
                subject: None,
                content: &turn.content,
                tags: None,
                source: &turn.role,
                importance: 0.5,
            })?;
        }
        let hits = recall(&reader, &input.question, None, 1, &[])?;
        Ok(hits.into_iter().next().map(|hit| hit.content))
    }
}

// Port of the LongMemEval QA evaluation loop (MIT, see her/THIRD_PARTY_NOTICES.md).
// Keep its question id, hypothesis and per-question label fields and _abs convention.
// The smoke runner uses an exact synthetic judge and never invokes a model.
fn run(
    input: impl Read,
    mut output: impl Write,
    adapter: &impl RecallAdapter,
) -> Result<(), Box<dyn Error>> {
    let questions: Vec<Question> = serde_json::from_reader(input)?;
    for question in questions {
        let answer = adapter.answer(&question.input)?;
        let label = if question.question_id.contains("_abs") {
            answer.is_none()
        } else {
            answer
                .as_deref()
                .is_some_and(|text| text.trim() == question.answer.trim())
        };
        serde_json::to_writer(
            &mut output,
            &json!({
                "question_id": question.question_id,
                "hypothesis": answer.unwrap_or_default(),
                "autoeval_label": {"model": "synthetic-exact", "label": label},
            }),
        )?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args_os().len() != 1 {
        return Err("usage: memory-bench < synthetic-questions.json".into());
    }
    run(io::stdin().lock(), io::stdout().lock(), &CurrentMemory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_adapter_prints_one_result_per_question() {
        let input = r#"[
            {
                "question_id":"orchard", "question_type":"single-session-user",
                "question":"Which orchard crate did we choose?",
                "answer":"We chose the green orchard crate.",
                "haystack_sessions":[[{"role":"user","content":"We chose the green orchard crate."}]]
            },
            {
                "question_id":"orchard_abs", "question_type":"single-session-user",
                "question":"Which orchard crate did we choose?",
                "answer":"No orchard crate was discussed.",
                "haystack_sessions":[[{"role":"assistant","content":"The observatory opens at dusk."}]]
            }
        ]"#;
        let mut output = Vec::new();
        run(input.as_bytes(), &mut output, &CurrentMemory).unwrap();
        let results: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&output)
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(results.len(), 2, "one result per question");
        assert_eq!(
            results[0]["hypothesis"], "We chose the green orchard crate.",
            "current Recall returns the matching memory"
        );
        assert_eq!(results[0]["question_id"], "orchard");
        assert_eq!(results[0]["autoeval_label"]["label"], true);
        assert_eq!(results[1]["question_id"], "orchard_abs");
        assert_eq!(results[1]["hypothesis"], "");
        assert_eq!(results[1]["autoeval_label"]["label"], true);
    }
}
