use rig::tool::PortableTool;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};

pub type QuestionSender = mpsc::Sender<QuestionRequest>;
pub type QuestionReceiver = mpsc::Receiver<QuestionRequest>;

#[derive(Debug)]
pub struct QuestionRequest {
    pub questions: Vec<QuestionItem>,
    pub reply: oneshot::Sender<QuestionResponse>,
}

#[derive(Debug)]
pub enum QuestionResponse {
    Answered(Vec<Vec<String>>),
    Rejected,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuestionArgs {
    pub questions: Vec<QuestionItem>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuestionItem {
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: Option<bool>,
    #[serde(default = "default_custom")]
    pub custom: bool,
}

fn default_custom() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

pub struct QuestionTool {
    pub question_tx: QuestionSender,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

impl QuestionTool {
    pub fn new(question_tx: QuestionSender) -> Self {
        Self {
            question_tx,
            permission: None,
            ask_tx: None,
        }
    }

    /// Builder for the production path: wires the permission checker
    /// and ask channel so `question` invocations go through the same
    /// allow/ask/deny rules as every other behaviour-altering tool.
    pub fn with_permission(
        mut self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        self.permission = permission;
        self.ask_tx = ask_tx;
        self
    }
}

impl PortableTool for QuestionTool {
    const NAME: &'static str = "question";

    type Error = ToolError;
    type Args = QuestionArgs;
    type Output = String;

    fn description(&self) -> String {
        "Ask the user structured questions during execution. Use this when you need to:\n\
                1. Gather user preferences or requirements\n\
                2. Clarify ambiguous instructions before proceeding\n\
                3. Get decisions on implementation choices as you work\n\
                4. Offer choices about what direction to take\n\
                \n\
                The tool blocks until the user answers. Multiple questions can be asked in one call.\n\
                \n\
                Usage notes:\n\
                - When `custom` is enabled (default), a \"Type your own answer\" option is added automatically; don't include \"Other\" or catch-all options yourself.\n\
                - Answers are returned per question (one array of selected labels each); set `multi_select: true` to allow more than one selection.\n\
                - If you recommend a specific option, make it the first option and add \" (Recommended)\" at the end of the label.\n\
                - Use `header` to group related questions under a short section title.\n\
                - Prefer asking over guessing when the user's request is genuinely ambiguous — but don't over-ask for clearly-decidable details."
                .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "List of questions to ask the user",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The question text"
                            },
                            "header": {
                                "type": "string",
                                "description": "Optional section heading displayed above the question"
                            },
                            "options": {
                                "type": "array",
                                "description": "Answer choices for the user",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "Short display label for the option"},
                                        "description": {"type": "string", "description": "Explanation of what this choice means"}
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "Allow selecting multiple options (default: false)"
                            },
                            "custom": {
                                "type": "boolean",
                                "description": "Whether to show a 'Type your own answer' option (default: true)"
                            }
                        },
                        "required": ["question", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn call(&self, args: QuestionArgs) -> Result<String, ToolError> {
        // Route through the same permission system as task / write /
        // bash. `question` rewrites what the LLM sees by injecting
        // user input — that's behavior-altering and shouldn't bypass
        // user rules.
        let summary = args
            .questions
            .first()
            .map(|q| q.question.clone())
            .unwrap_or_default();
        check_perm(&self.permission, &self.ask_tx, "question", &summary).await?;

        let (reply_tx, reply_rx) = oneshot::channel();

        self.question_tx
            .send(QuestionRequest {
                questions: args.questions.clone(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| ToolError::Msg("question system unavailable".to_string()))?;

        // dirge-8cbm: asking the user IS this tool's work, and it has no
        // idea how long they will take. Mark the wait so the dispatch
        // watchdog does not cut the call out from under them.
        let response = {
            let _waiting = crate::human_wait::HumanWait::begin();
            reply_rx.await
        };
        match response {
            Ok(QuestionResponse::Answered(answers)) => {
                let mut output = String::new();
                // Audit L13: zip(answers) silently dropped trailing
                // questions when the UI returned fewer answer arrays
                // than questions (partial response, plugin override
                // shape mismatch). Iterate by question index so
                // every question surfaces; missing answers are
                // explicitly marked `[no answer]` so the LLM sees
                // the gap and the user knows what slipped through.
                for (i, item) in args.questions.iter().enumerate() {
                    if i > 0 {
                        output.push_str("\n\n");
                    }
                    if let Some(header) = &item.header {
                        output.push_str(&format!("## {}\n", header));
                    }
                    let answer_text = match answers.get(i) {
                        Some(a) if !a.is_empty() => a.join(", "),
                        _ => "[no answer]".to_string(),
                    };
                    output.push_str(&format!(
                        "**Q{}:** {}\n**A:** {}",
                        i + 1,
                        item.question,
                        answer_text,
                    ));
                }
                if output.is_empty() {
                    output = "(no questions answered)".to_string();
                }
                Ok(output)
            }
            Ok(QuestionResponse::Rejected) => {
                Err(ToolError::Msg("question rejected by user".to_string()))
            }
            Err(_) => Err(ToolError::Msg(
                "question channel closed unexpectedly".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let (tx, _rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);
        let def = rig::tool::tool_definition(&tool);
        assert_eq!(def.name, "question");
    }

    #[tokio::test]
    async fn test_single_select_question() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "What color?".to_string(),
                header: None,
                options: vec![
                    QuestionOption {
                        label: "Red".to_string(),
                        description: "Red color".to_string(),
                    },
                    QuestionOption {
                        label: "Blue".to_string(),
                        description: "Blue color".to_string(),
                    },
                ],
                multi_select: None,
                custom: false,
            }],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        assert_eq!(req.questions.len(), 1);
        assert_eq!(req.questions[0].question, "What color?");

        let _ = req
            .reply
            .send(QuestionResponse::Answered(vec![vec!["Red".to_string()]]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("**Q1:** What color?"));
        assert!(result.contains("**A:** Red"));
    }

    #[tokio::test]
    async fn test_multi_select_question() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "Pick colors".to_string(),
                header: Some("Colors".to_string()),
                options: vec![
                    QuestionOption {
                        label: "Red".to_string(),
                        description: "".to_string(),
                    },
                    QuestionOption {
                        label: "Blue".to_string(),
                        description: "".to_string(),
                    },
                ],
                multi_select: Some(true),
                custom: false,
            }],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(QuestionResponse::Answered(vec![vec![
            "Red".to_string(),
            "Blue".to_string(),
        ]]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("## Colors"));
        assert!(result.contains("**A:** Red, Blue"));
    }

    #[tokio::test]
    async fn test_reject_returns_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "What?".to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "A".to_string(),
                    description: "".to_string(),
                }],
                multi_select: None,
                custom: false,
            }],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(QuestionResponse::Rejected);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rejected"));
    }

    #[tokio::test]
    async fn test_multiple_questions() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![
                QuestionItem {
                    question: "Q1".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A1".to_string(),
                        description: "".to_string(),
                    }],
                    multi_select: None,
                    custom: false,
                },
                QuestionItem {
                    question: "Q2".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A2".to_string(),
                        description: "".to_string(),
                    }],
                    multi_select: None,
                    custom: false,
                },
            ],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        assert_eq!(req.questions.len(), 2);
        let _ = req.reply.send(QuestionResponse::Answered(vec![
            vec!["A1".to_string()],
            vec!["A2".to_string()],
        ]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("Q1"));
        assert!(result.contains("Q2"));
        assert!(result.contains("A1"));
        assert!(result.contains("A2"));
    }

    // Partial answers (user Esc on Q2 after answering Q1) must preserve
    // what was answered and mark the rest [no answer], not error.
    #[tokio::test]
    async fn partial_answers_preserve_answered_questions() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![
                QuestionItem {
                    question: "Q1".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A1".to_string(),
                        description: "".to_string(),
                    }],
                    multi_select: None,
                    custom: false,
                },
                QuestionItem {
                    question: "Q2".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A2".to_string(),
                        description: "".to_string(),
                    }],
                    multi_select: None,
                    custom: false,
                },
            ],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        // Only Q1 answered — Q2 escaped
        let _ = req
            .reply
            .send(QuestionResponse::Answered(vec![vec!["A1".to_string()]]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("**Q1:** Q1"));
        assert!(result.contains("**A:** A1"));
        assert!(result.contains("**Q2:** Q2"));
        assert!(result.contains("**A:** [no answer]"));
    }

    // Header text must be emitted as a markdown `## …` heading so the agent
    // sees structured output rather than a flat blob.
    #[tokio::test]
    async fn output_includes_header_as_markdown_heading() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);
        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "Which?".into(),
                header: Some("Choice".into()),
                options: vec![QuestionOption {
                    label: "A".into(),
                    description: "".into(),
                }],
                multi_select: None,
                custom: false,
            }],
        };
        let handle = tokio::spawn(async move { tool.call(args).await });
        let req = rx.recv().await.unwrap();
        let _ = req
            .reply
            .send(QuestionResponse::Answered(vec![vec!["A".into()]]));
        let out = handle.await.unwrap().unwrap();
        assert!(out.contains("## Choice"), "got: {out}");
        assert!(out.contains("**Q1:** Which?"));
        assert!(out.contains("**A:** A"));
    }

    // Regression: dropping the receiver (channel closed before tool call)
    // must error cleanly. Don't panic, don't hang.
    #[tokio::test]
    async fn errors_when_channel_unavailable() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let tool = QuestionTool::new(tx);
        let result = tool
            .call(QuestionArgs {
                questions: vec![QuestionItem {
                    question: "Q?".into(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A".into(),
                        description: "".into(),
                    }],
                    multi_select: None,
                    custom: false,
                }],
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unavailable"));
    }

    // Reply oneshot dropped without sending → tool surfaces channel-closed
    // error rather than hanging.
    #[tokio::test]
    async fn errors_when_reply_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);
        let handle = tokio::spawn(async move {
            tool.call(QuestionArgs {
                questions: vec![QuestionItem {
                    question: "Q?".into(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A".into(),
                        description: "".into(),
                    }],
                    multi_select: None,
                    custom: false,
                }],
            })
            .await
        });
        let req = rx.recv().await.unwrap();
        drop(req.reply);
        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("channel closed"));
    }

    // Default for `custom` is true. Without serde reading the schema default
    // an agent that omits the field would get `false`, which silently changes
    // the picker UX.
    #[test]
    fn args_default_custom_is_true() {
        let parsed: QuestionItem = serde_json::from_value(serde_json::json!({
            "question": "Q?",
            "options": [{"label": "A", "description": ""}],
        }))
        .unwrap();
        assert!(parsed.custom);
    }

    // Multi-select with multiple answers must comma-join.
    #[tokio::test]
    async fn multi_select_joins_answers_with_comma() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);
        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "Pick".into(),
                header: None,
                options: vec![
                    QuestionOption {
                        label: "A".into(),
                        description: "".into(),
                    },
                    QuestionOption {
                        label: "B".into(),
                        description: "".into(),
                    },
                    QuestionOption {
                        label: "C".into(),
                        description: "".into(),
                    },
                ],
                multi_select: Some(true),
                custom: false,
            }],
        };
        let handle = tokio::spawn(async move { tool.call(args).await });
        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(QuestionResponse::Answered(vec![vec![
            "A".into(),
            "C".into(),
        ]]));
        let out = handle.await.unwrap().unwrap();
        assert!(out.contains("**A:** A, C"), "got: {out}");
    }
}

#[cfg(test)]
mod human_wait_tests {
    use super::*;

    /// dirge-8cbm: the wiring, not the mechanism. `HumanWait` is what
    /// stops the dispatch watchdog cutting a call mid-prompt, and it is
    /// worth nothing if the sites that actually wait forget to use it.
    /// This tool exists to ask a person and wait; while it is waiting,
    /// the count must say so.
    #[tokio::test]
    async fn asking_the_user_marks_the_call_as_waiting_on_a_person() {
        let _gate = crate::human_wait::TEST_GATE.lock().await;
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);
        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "Ship it?".to_string(),
                header: None,
                options: vec![
                    QuestionOption {
                        label: "Yes".to_string(),
                        description: "ship".to_string(),
                    },
                    QuestionOption {
                        label: "No".to_string(),
                        description: "hold".to_string(),
                    },
                ],
                multi_select: None,
                custom: false,
            }],
        };
        assert!(!crate::human_wait::anyone_waiting());

        let handle = tokio::spawn(async move { tool.call(args).await });
        let req = rx.recv().await.expect("the tool must ask");
        // The request is out and unanswered — this is precisely the
        // stretch the watchdog must not count.
        assert!(
            crate::human_wait::anyone_waiting(),
            "the question tool waited on the user without marking it"
        );

        let _ = req
            .reply
            .send(QuestionResponse::Answered(vec![vec!["Yes".to_string()]]));
        let _ = handle.await.expect("join");
        assert!(
            !crate::human_wait::anyone_waiting(),
            "the mark must clear once the user answers"
        );
    }
}
