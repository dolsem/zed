//! The status an agent reports at the end of each turn, as a trailing
//! `<turn_status>` tag in its final message.
//!
//! Zed asks for the tag in the agent's system prompt, hides it from the
//! transcript while the message streams, and asks again with a hidden prompt
//! when a turn ends without a valid one.

use serde::{Deserialize, Serialize};

pub const TURN_STATUS_OPEN_TAG: &str = "<turn_status>";
pub const TURN_STATUS_CLOSE_TAG: &str = "</turn_status>";

/// How many hidden prompts Zed sends for one turn to get a valid tag.
pub const MAX_TURN_STATUS_REPAIRS: usize = 2;

/// Starts every repair prompt, so that a replayed session can recognize and
/// hide the prompts it sent.
pub const TURN_STATUS_REPAIR_MARKER: &str = "[zed:turn_status_repair]";

pub const TURN_STATUS_INSTRUCTIONS: &str = r#"# Turn status

Zed shows the user whether you are done, need a decision from them, or could not finish something. End the final message of every turn with a turn status tag, as the very last thing you write:

<turn_status>{"status": "done", "pending_decisions": [], "incomplete": []}</turn_status>

The tag holds one JSON object:
- "status": one of
  - "done": everything requested was completed (and verified, where verification was possible), and nothing is waiting on the user.
  - "needs_decision": you are waiting on the user for at least one decision. List each one in "pending_decisions".
  - "partial": some of the requested work could not be done or verified, and the rest was done. List what was not done in "incomplete".
  - "blocked": none of the requested work could proceed. List what blocked it in "incomplete".
- "pending_decisions": the questions you asked the user in this message that you are waiting on, one short sentence each.
- "incomplete": one {"task": "...", "reason": "..."} object for each part of the task that could not be done or verified, with the concrete reason (e.g. {"task": "Test the page in the browser", "reason": "the browser MCP server is offline"}).

When is a decision pending:
- When you are blocked on something only the user can decide, or when you found something outside the task that they would likely want handled (e.g. "I also found a bug in parse_config, do you want me to fix it?"). Ask about it in your message and list it.
- Do not offer optional follow-ups such as "Say the word and I'll commit" or "Want me to also...?" for routine next steps. If a step was part of the task, do it; if it was not, do not offer it.
- Ask for confirmation of a step that was part of the task only when something unexpected changed its risk or meaning (e.g. you were asked to commit, but the diff contains changes you did not expect, or the tests fail), and say what was unexpected.

Never report work or verification you did not do. Write the tag only once, at the end of the turn's final message, and never in the middle of a message or before a tool call."#;

pub fn turn_status_repair_prompt(problem: &str) -> String {
    format!(
        "{TURN_STATUS_REPAIR_MARKER} Your last message did not end with a valid turn status tag ({problem}). \
         Reply with only the `<turn_status>` tag for the turn you just finished, following the turn status \
         instructions, and nothing else. Do not call any tools and do not repeat your previous message."
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Done,
    NeedsDecision,
    Partial,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteTask {
    pub task: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStatus {
    pub status: TurnOutcome,
    #[serde(default)]
    pub pending_decisions: Vec<String>,
    #[serde(default)]
    pub incomplete: Vec<IncompleteTask>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnStatusParse {
    Valid(TurnStatus),
    /// The tag is there, but its content is not a valid status.
    Malformed(String),
    /// No tag ends the message.
    Missing,
}

impl TurnStatusParse {
    /// What was wrong, for the repair prompt. `None` for a valid status.
    pub fn problem(&self) -> Option<String> {
        match self {
            TurnStatusParse::Valid(_) => None,
            TurnStatusParse::Malformed(error) => Some(error.clone()),
            TurnStatusParse::Missing => Some("the tag is missing".to_string()),
        }
    }
}

/// Parses `block`, which starts with [`TURN_STATUS_OPEN_TAG`]. Returns
/// `None` when the block is not a trailing tag: it has no closing tag, or text
/// follows it, so it is prose that mentions the tag.
fn parse_tag_block(block: &str) -> Option<TurnStatusParse> {
    let inner = block.strip_prefix(TURN_STATUS_OPEN_TAG)?;
    let close = inner.find(TURN_STATUS_CLOSE_TAG)?;
    if !inner[close + TURN_STATUS_CLOSE_TAG.len()..]
        .trim()
        .is_empty()
    {
        return None;
    }
    let json = inner[..close].trim();
    let json = json
        .strip_prefix("```json")
        .or_else(|| json.strip_prefix("```"))
        .and_then(|json| json.strip_suffix("```"))
        .map_or(json, str::trim);
    Some(match serde_json::from_str::<TurnStatus>(json) {
        Ok(status) => match validate(&status) {
            Ok(()) => TurnStatusParse::Valid(status),
            Err(error) => TurnStatusParse::Malformed(error),
        },
        Err(error) => TurnStatusParse::Malformed(format!("its content is not valid JSON: {error}")),
    })
}

fn validate(status: &TurnStatus) -> Result<(), String> {
    match status.status {
        TurnOutcome::Done if !status.pending_decisions.is_empty() => {
            Err("\"done\" can't have pending decisions, use \"needs_decision\"".into())
        }
        TurnOutcome::Done if !status.incomplete.is_empty() => {
            Err("\"done\" can't have incomplete tasks, use \"partial\" or \"blocked\"".into())
        }
        TurnOutcome::NeedsDecision if status.pending_decisions.is_empty() => {
            Err("\"needs_decision\" needs at least one pending decision".into())
        }
        TurnOutcome::Partial | TurnOutcome::Blocked if status.incomplete.is_empty() => {
            Err("\"partial\" and \"blocked\" need at least one incomplete task".into())
        }
        _ => Ok(()),
    }
}

/// Separates a trailing turn status tag from the text of a streaming message.
///
/// Text that could be the start of the tag is held back until the next chunk
/// shows whether it is. Once the tag starts, the rest of the message is
/// captured instead of shown. If the message goes on after it (more text after
/// the closing tag, or a tool call), the captured text was prose and is shown
/// after all.
#[derive(Default)]
pub(crate) struct TurnStatusFilter {
    held: String,
    captured: Option<String>,
}

impl TurnStatusFilter {
    /// Takes the next chunk of message text and returns the part to show.
    pub fn push(&mut self, text: &str) -> String {
        if let Some(captured) = &mut self.captured {
            captured.push_str(text);
            return String::new();
        }
        let mut buffer = std::mem::take(&mut self.held);
        buffer.push_str(text);
        if let Some(start) = buffer.find(TURN_STATUS_OPEN_TAG) {
            self.captured = Some(buffer[start..].to_string());
            buffer.truncate(start);
            return buffer;
        }
        // The tag is ASCII, so the held suffix starts at a char boundary.
        let held_len = (1..TURN_STATUS_OPEN_TAG.len())
            .rev()
            .find(|len| buffer.ends_with(&TURN_STATUS_OPEN_TAG[..*len]))
            .unwrap_or(0);
        self.held = buffer.split_off(buffer.len() - held_len);
        buffer
    }

    /// The message continued with something other than text, so nothing
    /// held or captured so far ends it. Returns that text, to show.
    pub fn interrupt(&mut self) -> String {
        let mut text = std::mem::take(&mut self.held);
        if let Some(captured) = self.captured.take() {
            text.push_str(&captured);
        }
        text
    }

    /// The message ended. Returns the text to show that was held back, and
    /// the status the message ended with.
    pub fn finish(&mut self) -> (String, TurnStatusParse) {
        let held = std::mem::take(&mut self.held);
        let Some(captured) = self.captured.take() else {
            return (held, TurnStatusParse::Missing);
        };
        match parse_tag_block(&captured) {
            Some(parse) => (held, parse),
            None => (held + &captured, TurnStatusParse::Missing),
        }
    }

    /// The status of the captured tag, if it is complete and valid. Lets a
    /// replayed session, which never finishes its last message, show its
    /// status.
    pub fn captured_status(&self) -> Option<TurnStatus> {
        match parse_tag_block(self.captured.as_deref()?)? {
            TurnStatusParse::Valid(status) => Some(status),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(chunks: &[&str]) -> (String, TurnStatusParse) {
        let mut filter = TurnStatusFilter::default();
        let mut shown = String::new();
        for chunk in chunks {
            shown.push_str(&filter.push(chunk));
        }
        let (rest, parse) = filter.finish();
        shown.push_str(&rest);
        (shown, parse)
    }

    #[test]
    fn test_trailing_tag_is_hidden_across_chunks() {
        let (shown, parse) = stream(&[
            "All done.\n\n<turn",
            "_status>{\"status\": \"done\"}</turn_",
            "status>\n",
        ]);
        assert_eq!(shown, "All done.\n\n");
        assert_eq!(
            parse,
            TurnStatusParse::Valid(TurnStatus {
                status: TurnOutcome::Done,
                pending_decisions: Vec::new(),
                incomplete: Vec::new(),
            })
        );
    }

    #[test]
    fn test_partial_tag_prefix_is_shown_when_not_a_tag() {
        let (shown, parse) = stream(&["a <tu", "rtle> b"]);
        assert_eq!(shown, "a <turtle> b");
        assert_eq!(parse, TurnStatusParse::Missing);
    }

    #[test]
    fn test_tag_in_prose_is_shown() {
        let (shown, parse) = stream(&["Use <turn_status> at the end. More text."]);
        assert_eq!(shown, "Use <turn_status> at the end. More text.");
        assert_eq!(parse, TurnStatusParse::Missing);
    }

    #[test]
    fn test_interrupt_shows_captured_text() {
        let mut filter = TurnStatusFilter::default();
        assert_eq!(filter.push("x <turn_status>{}</turn_status>"), "x ");
        assert_eq!(filter.interrupt(), "<turn_status>{}</turn_status>");
        assert_eq!(filter.finish(), (String::new(), TurnStatusParse::Missing));
    }

    #[test]
    fn test_inconsistent_status_is_malformed() {
        let (shown, parse) =
            stream(&["<turn_status>{\"status\": \"needs_decision\"}</turn_status>"]);
        assert_eq!(shown, "");
        assert!(matches!(parse, TurnStatusParse::Malformed(_)));
    }

    #[test]
    fn test_fenced_json_is_accepted() {
        let (_, parse) = stream(&[
            "<turn_status>```json\n{\"status\": \"blocked\", \"incomplete\": [{\"task\": \"t\", \"reason\": \"r\"}]}\n```</turn_status>",
        ]);
        assert!(
            matches!(parse, TurnStatusParse::Valid(status) if status.status == TurnOutcome::Blocked)
        );
    }
}
