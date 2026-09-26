// How a conversation is told to a CLI that reads one prompt rather than a
// request: the words of each message as text with the calls and results in
// it named where they happened, and the images as blocks of their own for
// the API to carry.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::ir::{Kind, Message, Part, Request, args_string};

// claude is the request as the blocks of one turn: what the caller's system
// says, then the conversation, with the labels a chat CLI reads for roles.
pub fn claude(req: &Request) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    if !req.system.is_empty()
        || req.tool_choice == "required"
        || req.tool_choice.starts_with("name:")
    {
        text.push_str("<external_system_instructions>\n");
        text.push_str(&req.system);
        if req.tool_choice == "required" {
            text.push_str("\nYou must call at least one available tool before answering.");
        } else if let Some(name) = req.tool_choice.strip_prefix("name:") {
            let _ = write!(text, "\nYou must call the {name} tool.");
        }
        text.push_str("\n</external_system_instructions>\n\n");
    }
    for message in &req.messages {
        let label = if message.role == "assistant" {
            "Assistant"
        } else {
            "Human"
        };
        text.push_str(label);
        text.push_str(": ");
        parts(&mut blocks, &mut text, &message.parts);
        text.push_str("\n\n");
    }
    closed(blocks, &mut text)
}

// claude_turn is the user's messages in a conversation the CLI already has,
// as it would have been told them itself.
pub fn claude_turn(messages: &[Message]) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    for (at, message) in messages.iter().enumerate() {
        if at > 0 {
            text.push_str("\n\n");
        }
        parts(&mut blocks, &mut text, &message.parts);
    }
    closed(blocks, &mut text)
}

// parts writes what each part says, keeping the images out of the text:
// they are no words, and go between the text blocks as they came.
fn parts(blocks: &mut Vec<Value>, text: &mut String, parts: &[Part]) {
    for part in parts {
        match part.kind {
            // a CLI reads reasoning as the words that came with it
            Kind::Text | Kind::Thinking => text.push_str(&part.text),
            Kind::ToolCall => {
                let _ = write!(
                    text,
                    "\n[tool call {} id={} args={}]",
                    part.name,
                    part.id,
                    args_string(part)
                );
            }
            Kind::ToolResult => {
                let error = if part.is_error { " error" } else { "" };
                let _ = write!(
                    text,
                    "\n[tool result id={}{}]\n{}",
                    part.call_id, error, part.text
                );
            }
            Kind::Image => {
                flush(blocks, text);
                if !part.data.is_empty() {
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": part.media_type,
                            "data": part.data,
                        }
                    }));
                } else if !part.url.is_empty() {
                    blocks.push(json!({
                        "type": "image",
                        "source": { "type": "url", "url": part.url }
                    }));
                }
            }
        }
    }
}

// closed is the blocks as the turn that was said, which is never nothing:
// a CLI asked to go on from an empty conversation is told so in words.
fn closed(mut blocks: Vec<Value>, text: &mut String) -> Vec<Value> {
    flush(&mut blocks, text);
    if blocks.is_empty() {
        blocks.push(text_block("[continue]"));
    }
    blocks
}

fn flush(blocks: &mut Vec<Value>, text: &mut String) {
    if text.is_empty() {
        return;
    }
    let said = std::mem::take(text);
    blocks.push(json!({ "type": "text", "text": said }));
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

#[cfg(test)]
mod tests {
    use super::super::ir::args_of;
    use super::*;

    fn part(kind: Kind, text: &str) -> Part {
        Part {
            kind,
            text: text.to_owned(),
            ..Part::default()
        }
    }

    fn message(role: &str, parts: Vec<Part>) -> Message {
        Message {
            role: role.to_owned(),
            parts,
        }
    }

    fn said(blocks: &[Value]) -> String {
        blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .map(|block| block["text"].as_str().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_system_and_its_turns_are_one_text() {
        let req = Request {
            system: "be brief".to_owned(),
            messages: vec![
                message("user", vec![part(Kind::Text, "hello")]),
                message("assistant", vec![part(Kind::Text, "hi")]),
            ],
            ..Request::default()
        };
        let blocks = claude(&req);
        assert_eq!(blocks.len(), 1);
        let text = said(&blocks);
        assert!(
            text.starts_with("<external_system_instructions>\nbe brief"),
            "{text}"
        );
        assert!(
            text.contains("brief\n</external_system_instructions>"),
            "{text}"
        );
        assert!(text.contains("Human: hello"), "{text}");
        assert!(text.contains("Assistant: hi"), "{text}");
    }

    #[test]
    fn a_call_that_was_demanded_is_demanded_in_words() {
        let mut req = Request {
            system: "s".to_owned(),
            tool_choice: "required".to_owned(),
            ..Request::default()
        };
        let demanded = "at least one available tool before answering.";
        assert!(said(&claude(&req)).contains(demanded), "{demanded}");
        req.tool_choice = "name:read_file".to_owned();
        assert!(said(&claude(&req)).contains("You must call the read_file tool."));
        req.tool_choice = "auto".to_owned();
        let text = said(&claude(&req));
        assert!(
            text.starts_with("<external_system_instructions>\ns\n</external_system_instructions>"),
            "{text}"
        );
    }

    #[test]
    fn no_words_at_all_still_asks_to_go_on() {
        assert_eq!(claude(&Request::default()), vec![text_block("[continue]")]);
    }

    #[test]
    fn an_image_divides_the_words_around_it() {
        let parts = [
            part(Kind::Text, "look "),
            Part {
                kind: Kind::Image,
                media_type: "image/png".to_owned(),
                data: "aGk=".to_owned(),
                ..Part::default()
            },
            part(Kind::Text, "at this"),
        ];
        let blocks = claude_turn(&[message("user", parts.into_iter().collect())]);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], text_block("look "));
        assert_eq!(blocks[1]["source"]["data"], json!("aGk="));
        assert_eq!(blocks[1]["source"]["media_type"], json!("image/png"));
        assert_eq!(blocks[2], text_block("at this"));
    }

    #[test]
    fn a_url_image_carries_its_url() {
        let parts = vec![Part {
            kind: Kind::Image,
            url: "https://x/y.png".to_owned(),
            ..Part::default()
        }];
        let blocks = claude_turn(&[message("user", parts)]);
        assert_eq!(blocks[0]["source"]["url"], json!("https://x/y.png"));
        assert_eq!(blocks[0]["source"]["type"], json!("url"));
    }

    #[test]
    fn calls_and_results_are_named_where_they_happened() {
        let parts = vec![
            Part {
                kind: Kind::ToolCall,
                name: "read".to_owned(),
                id: "call-1".to_owned(),
                args: Some(json!({"path": "/tmp/x"})),
                ..Part::default()
            },
            Part {
                kind: Kind::ToolResult,
                call_id: "call-1".to_owned(),
                text: "one line".to_owned(),
                is_error: true,
                ..Part::default()
            },
        ];
        let text = said(&claude_turn(&[message("user", parts)]));
        assert_eq!(
            text,
            concat!(
                "\n[tool call read id=call-1 args={\"path\":\"/tmp/x\"}]",
                "\n[tool result id=call-1 error]\none line"
            )
        );
    }

    #[test]
    fn reasoning_reads_as_the_words_it_came_with() {
        let parts = vec![part(Kind::Thinking, "hmm "), part(Kind::Text, "then this")];
        assert_eq!(
            said(&claude_turn(&[message("user", parts)])),
            "hmm then this"
        );
    }

    #[test]
    fn a_turn_of_its_own_joins_what_the_user_said() {
        let blocks = claude_turn(&[
            message("user", vec![part(Kind::Text, "first")]),
            message("user", vec![part(Kind::Text, "second")]),
        ]);
        assert_eq!(said(&blocks), "first\n\nsecond");
        assert_eq!(args_of(&Part::default()), json!({}));
    }
}
