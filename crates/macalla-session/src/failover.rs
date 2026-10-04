// SPDX-License-Identifier: MIT
//! The cloud pool's failover: several cleanup models, asked in the order the
//! person ticked them, the next taking over when one fails.
//!
//! A failure is an error from the request itself: the network, an HTTP
//! status, a refused key, a timeout, a reply that could not be read. A reply
//! the provider's guard turned back, or one that changes nothing, is an
//! answer, and the pass ends there. A member that says it cannot serve right
//! now is passed over without being asked.
//!
//! Nothing is remembered between dictations: every pass starts from the first
//! ticked model. Each member keeps its own request timeout, and the pass ends
//! after the last member.
//!
//! Before asking the second member, and each one after it, the pass checks
//! whether the dictation it serves has been superseded by the next one or let
//! go, and stops if so. Otherwise a dead first provider could keep a stale
//! document alive through the whole pool.
//!
//! The loop does not care what kind of model it asks: a cleanup model, or a
//! model asked for free text, such as a meeting summary. Each kind is a
//! [`Candidate`], and every kind is asked by the same rules.

use macalla_core::block::Block;
use macalla_core::enhance::{Enhancer, EnhancerCaps};
use std::sync::atomic::{AtomicBool, Ordering};

/// A model a pool can ask, of whatever kind, with the names its log lines
/// carry.
pub(crate) trait Candidate {
    /// The provider's id, for the log line when this member fails or answers.
    fn provider(&self) -> &str;
    /// The model's id, for the same log line.
    fn model(&self) -> &str;
    /// Whether it can serve right now. One that cannot is passed over without
    /// being asked.
    fn ready(&self) -> bool;
}

/// One ticked cleanup model, built.
pub(crate) struct Member {
    pub(crate) enhancer: Box<dyn Enhancer>,
    /// The provider's id, for the log line when this member fails or answers.
    pub(crate) provider: String,
    /// The model's id, for the same log line.
    pub(crate) model: String,
}

impl Candidate for Member {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn ready(&self) -> bool {
        self.enhancer.is_available()
    }
}

/// Ticked models in order, the first that answers used. Never empty.
pub(crate) struct Failover<T> {
    members: Vec<T>,
    work: Work,
}

/// What a pool is asked for, so its log lines and its error say which pool
/// it is: a summary must never tell a person that the cleanup pool failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Work {
    Cleanup,
    Summary,
}

impl Work {
    fn name(self) -> &'static str {
        match self {
            Work::Cleanup => "cleanup",
            Work::Summary => "summary",
        }
    }

    /// Why a pass stopped early, for the log line.
    fn stopped(self) -> &'static str {
        match self {
            Work::Cleanup => "the dictation was superseded or let go",
            Work::Summary => "the summary was stopped",
        }
    }
}

impl<T: Candidate> Failover<T> {
    /// A cleanup pool.
    pub(crate) fn new(members: Vec<T>) -> Self {
        Self::doing(Work::Cleanup, members)
    }

    /// A pool asked for `work`.
    pub(crate) fn doing(work: Work, members: Vec<T>) -> Self {
        debug_assert!(!members.is_empty(), "a pool has at least one member");
        Self { members, work }
    }

    /// Whether any member can serve right now.
    pub(crate) fn is_available(&self) -> bool {
        self.members.iter().any(Candidate::ready)
    }

    /// Ask the members in order through `call` until one answers. `stop` is
    /// the flag of the work the pass serves: set, it ends the pass before the
    /// next member is asked. The error after a pass with no answer is the last
    /// member's own.
    ///
    /// Log lines name the provider and model, and a failure's kind. Never a
    /// failure's message, which can carry a reply's body, and never the text.
    pub(crate) fn ask<R>(
        &self,
        stop: &AtomicBool,
        mut call: impl FnMut(&T) -> macalla_core::Result<R>,
    ) -> macalla_core::Result<R> {
        let work = self.work.name();
        let mut failure = None;
        for (at, member) in self.members.iter().enumerate() {
            if at > 0 && stop.load(Ordering::Relaxed) {
                tracing::info!(
                    provider = %member.provider(),
                    "{work} pool stopped: {}",
                    self.work.stopped()
                );
                break;
            }
            if !member.ready() {
                tracing::info!(
                    provider = %member.provider(),
                    model = %member.model(),
                    "{work} pool member cannot serve now; trying the next"
                );
                continue;
            }
            match call(member) {
                Ok(answer) => {
                    tracing::info!(
                        provider = %member.provider(),
                        model = %member.model(),
                        "{work} pool member answered"
                    );
                    return Ok(answer);
                }
                Err(error) => {
                    tracing::info!(
                        provider = %member.provider(),
                        model = %member.model(),
                        error = error_kind(&error),
                        "{work} pool member failed; trying the next"
                    );
                    failure = Some(error);
                }
            }
        }
        Err(failure.unwrap_or_else(|| macalla_core::Error::Enhance {
            backend: "pool",
            message: format!("no model in the {work} pool answered"),
        }))
    }
}

impl Failover<Member> {
    /// The first ticked member's capabilities, for logs.
    pub(crate) fn caps(&self) -> EnhancerCaps {
        self.members[0].enhancer.caps()
    }

    /// Clean up `blocks` with the first member that answers ([`Failover::ask`]).
    pub(crate) fn enhance(
        &self,
        blocks: &[Block],
        terms: &[String],
        stop: &AtomicBool,
    ) -> macalla_core::Result<Vec<Block>> {
        self.ask(stop, |member| member.enhancer.enhance(blocks, terms))
    }
}

/// A failure's kind, for a log line: a fixed label, never the message, which
/// can carry a reply's body.
fn error_kind(error: &macalla_core::Error) -> &'static str {
    match error {
        macalla_core::Error::Audio(_) => "audio",
        macalla_core::Error::Stt { .. } => "transcription",
        macalla_core::Error::Enhance { message, .. } => enhance_kind(message),
        macalla_core::Error::Unsupported { .. } => "unsupported",
        macalla_core::Error::SchemaVersion { .. } => "schema version",
        macalla_core::Error::History(_) => "history",
        macalla_core::Error::Model(_) => "model file",
    }
}

/// A cleanup request's failure, read from the fixed wording the enhancers
/// use for it: "the request to {url} failed: {cause}", where the cause is
/// ureq's own error text ("http status: 401", "timeout: ...", "io: ..."),
/// or "reading the reply from {url}: ..." and "the reply from {url} had no
/// text in it". The labels are:
///
/// - `auth`: the provider refused the key (401 or 403);
/// - `http`: any other status;
/// - `timeout`: the request ran out of time;
/// - `reply`: an answer came, but could not be read or had no text;
/// - `network`: no answer came at all.
///
/// A message worded any other way is `request`: if an enhancer's wording
/// changes, the log line loses detail, and nothing else does.
fn enhance_kind(message: &str) -> &'static str {
    if message.starts_with("reading the reply from ") || message.ends_with(" had no text in it") {
        return "reply";
    }
    // A URL holds no space, so the first " failed: " ends it.
    let Some((_, cause)) = message
        .strip_prefix("the request to ")
        .and_then(|rest| rest.split_once(" failed: "))
    else {
        return "request";
    };
    match cause.strip_prefix("http status: ") {
        Some(status) if matches!(status.trim(), "401" | "403") => "auth",
        Some(_) => "http",
        None if cause.starts_with("timeout: ") => "timeout",
        None => "network",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A failure worded as the enhancers word it: the request's own
    /// failure carries ureq's error text after "failed: ".
    fn request_failed(cause: &str) -> macalla_core::Error {
        macalla_core::Error::Enhance {
            backend: "openai-compatible",
            message: format!(
                "the request to https://api.example.com/v1/chat/completions failed: {cause}"
            ),
        }
    }

    #[test]
    fn a_refused_key_is_logged_as_auth() {
        assert_eq!(error_kind(&request_failed("http status: 401")), "auth");
        assert_eq!(error_kind(&request_failed("http status: 403")), "auth");
    }

    #[test]
    fn any_other_status_is_logged_as_http() {
        assert_eq!(error_kind(&request_failed("http status: 429")), "http");
        assert_eq!(error_kind(&request_failed("http status: 503")), "http");
    }

    #[test]
    fn a_request_that_ran_out_of_time_is_logged_as_timeout() {
        assert_eq!(error_kind(&request_failed("timeout: global")), "timeout");
    }

    #[test]
    fn a_reply_that_could_not_be_read_or_had_no_text_is_logged_as_reply() {
        let reply = |message: &str| macalla_core::Error::Enhance {
            backend: "anthropic",
            message: message.to_string(),
        };
        assert_eq!(
            error_kind(&reply(
                "reading the reply from https://api.example.com/v1/messages: too long"
            )),
            "reply"
        );
        assert_eq!(
            error_kind(&reply(
                "the reply from https://api.example.com/v1/messages had no text in it"
            )),
            "reply"
        );
    }

    #[test]
    fn a_request_that_never_got_an_answer_is_logged_as_network() {
        assert_eq!(
            error_kind(&request_failed("io: Connection refused (os error 61)")),
            "network"
        );
        assert_eq!(error_kind(&request_failed("host not found")), "network");
    }

    enum Answer {
        Fail,
        Unchanged,
        Changed(&'static str),
    }

    /// A model that counts its requests, answers as told, and can set the
    /// dictation's stop flag while it runs, as the next dictation superseding
    /// this one would.
    struct FakeModel {
        available: bool,
        answer: Answer,
        asked: Arc<AtomicUsize>,
        stop_while_running: Option<Arc<AtomicBool>>,
    }

    impl Enhancer for FakeModel {
        fn caps(&self) -> EnhancerCaps {
            EnhancerCaps {
                id: "fake",
                typical_ms: None,
                constrained_output: false,
            }
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn enhance(&self, blocks: &[Block], _terms: &[String]) -> macalla_core::Result<Vec<Block>> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            if let Some(stop) = &self.stop_while_running {
                stop.store(true, Ordering::Relaxed);
            }
            match &self.answer {
                Answer::Fail => Err(macalla_core::Error::Enhance {
                    backend: "fake",
                    message: "the server answered 503".to_string(),
                }),
                Answer::Unchanged => Ok(blocks.to_vec()),
                Answer::Changed(text) => Ok(vec![paragraph(text)]),
            }
        }
    }

    fn paragraph(text: &str) -> Block {
        Block::Paragraph {
            text: text.to_string(),
        }
    }

    fn heard() -> Vec<Block> {
        vec![paragraph("cube control ran")]
    }

    /// A member, and the count of the requests it was sent.
    fn member(provider: &str, available: bool, answer: Answer) -> (Member, Arc<AtomicUsize>) {
        member_stopping(provider, available, answer, None)
    }

    fn member_stopping(
        provider: &str,
        available: bool,
        answer: Answer,
        stop_while_running: Option<Arc<AtomicBool>>,
    ) -> (Member, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        let enhancer = FakeModel {
            available,
            answer,
            asked: Arc::clone(&asked),
            stop_while_running,
        };
        (
            Member {
                enhancer: Box::new(enhancer),
                provider: provider.to_string(),
                model: format!("{provider}-small"),
            },
            asked,
        )
    }

    fn asked(count: &Arc<AtomicUsize>) -> usize {
        count.load(Ordering::Relaxed)
    }

    #[test]
    fn the_next_member_answers_when_the_first_fails() {
        let (first, first_asked) = member("openai", true, Answer::Fail);
        let (second, second_asked) = member("anthropic", true, Answer::Changed("kubectl ran."));
        let pool = Failover::new(vec![first, second]);

        let reply = pool.enhance(&heard(), &[], &AtomicBool::new(false));

        assert_eq!(reply.unwrap(), vec![paragraph("kubectl ran.")]);
        assert_eq!((asked(&first_asked), asked(&second_asked)), (1, 1));
    }

    #[test]
    fn an_unchanged_reply_is_an_answer_and_no_one_else_is_asked() {
        let (first, first_asked) = member("openai", true, Answer::Unchanged);
        let (second, second_asked) = member("anthropic", true, Answer::Changed("kubectl ran."));
        let pool = Failover::new(vec![first, second]);

        let reply = pool.enhance(&heard(), &[], &AtomicBool::new(false));

        assert_eq!(reply.unwrap(), heard());
        assert_eq!((asked(&first_asked), asked(&second_asked)), (1, 0));
    }

    #[test]
    fn every_member_failing_is_an_error_after_each_was_asked_once() {
        let (first, first_asked) = member("openai", true, Answer::Fail);
        let (second, second_asked) = member("anthropic", true, Answer::Fail);
        let pool = Failover::new(vec![first, second]);

        let reply = pool.enhance(&heard(), &[], &AtomicBool::new(false));

        assert!(reply.is_err(), "{reply:?}");
        assert_eq!((asked(&first_asked), asked(&second_asked)), (1, 1));
    }

    #[test]
    fn a_dictation_let_go_while_a_member_runs_stops_the_pass() {
        let stop = Arc::new(AtomicBool::new(false));
        let (first, first_asked) =
            member_stopping("openai", true, Answer::Fail, Some(Arc::clone(&stop)));
        let (second, second_asked) = member("anthropic", true, Answer::Changed("kubectl ran."));
        let pool = Failover::new(vec![first, second]);

        let reply = pool.enhance(&heard(), &[], &stop);

        assert!(reply.is_err(), "{reply:?}");
        assert_eq!(
            (asked(&first_asked), asked(&second_asked)),
            (1, 0),
            "the next member is not asked once the dictation is let go"
        );

        // Checked between members only: a pass whose dictation was already
        // let go still asks the first, as a single provider would be asked.
        let (first, first_asked) = member("openai", true, Answer::Fail);
        let (second, second_asked) = member("anthropic", true, Answer::Changed("kubectl ran."));
        let pool = Failover::new(vec![first, second]);
        let reply = pool.enhance(&heard(), &[], &AtomicBool::new(true));
        assert!(reply.is_err(), "{reply:?}");
        assert_eq!((asked(&first_asked), asked(&second_asked)), (1, 0));
    }

    #[test]
    fn the_pool_is_available_when_any_member_is() {
        let (first, _) = member("openai", false, Answer::Fail);
        let (second, _) = member("anthropic", true, Answer::Fail);
        assert!(Failover::new(vec![first, second]).is_available());

        let (first, _) = member("openai", false, Answer::Fail);
        let (second, _) = member("anthropic", false, Answer::Fail);
        assert!(!Failover::new(vec![first, second]).is_available());
    }

    /// A pool member that is not a cleanup model: it answers with a name, as
    /// any other kind of model a pool might hold would, through the same loop.
    struct Named {
        provider: &'static str,
        ready: bool,
    }

    impl Candidate for Named {
        fn provider(&self) -> &str {
            self.provider
        }
        fn model(&self) -> &str {
            "named-model"
        }
        fn ready(&self) -> bool {
            self.ready
        }
    }

    #[test]
    fn any_kind_of_member_is_asked_by_the_same_rules() {
        let pool = Failover::new(vec![
            Named {
                provider: "apple-intelligence",
                ready: false,
            },
            Named {
                provider: "openai",
                ready: true,
            },
            Named {
                provider: "anthropic",
                ready: true,
            },
            Named {
                provider: "mistral",
                ready: true,
            },
        ]);
        let mut asked = Vec::new();
        let reply = pool.ask(&AtomicBool::new(false), |member| {
            asked.push(member.provider);
            match member.provider {
                "openai" => Err(macalla_core::Error::Enhance {
                    backend: "fake",
                    message: "the server answered 503".to_string(),
                }),
                other => Ok(format!("answered by {other}")),
            }
        });
        assert_eq!(reply.unwrap(), "answered by anthropic");
        assert_eq!(
            asked,
            vec!["openai", "anthropic"],
            "passed over the one not ready, moved on after the failure, stopped at the answer"
        );

        // The stop flag is checked before the second member, as for cleanup:
        // a pass already let go still asks the first.
        let pool = Failover::new(vec![
            Named {
                provider: "openai",
                ready: true,
            },
            Named {
                provider: "anthropic",
                ready: true,
            },
        ]);
        let mut asked = Vec::new();
        let reply = pool.ask(&AtomicBool::new(true), |member| {
            asked.push(member.provider);
            Err::<String, _>(macalla_core::Error::Enhance {
                backend: "fake",
                message: "the server answered 503".to_string(),
            })
        });
        assert!(reply.is_err());
        assert_eq!(asked, vec!["openai"]);
    }

    #[test]
    fn a_summary_pool_that_no_model_answered_says_it_is_the_summary_pool() {
        let pool = Failover::doing(
            Work::Summary,
            vec![
                Named {
                    provider: "openai",
                    ready: false,
                },
                Named {
                    provider: "anthropic",
                    ready: false,
                },
            ],
        );
        let error = pool
            .ask(
                &AtomicBool::new(false),
                |_| Ok::<_, macalla_core::Error>(()),
            )
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("summary pool"), "{message}");
        assert!(!message.contains("cleanup"), "{message}");
    }

    #[test]
    fn a_member_that_cannot_serve_now_is_passed_over_without_being_asked() {
        let (first, first_asked) = member("openai", false, Answer::Changed("never sent"));
        let (second, second_asked) = member("anthropic", true, Answer::Changed("kubectl ran."));
        let pool = Failover::new(vec![first, second]);

        let reply = pool.enhance(&heard(), &[], &AtomicBool::new(false));

        assert_eq!(reply.unwrap(), vec![paragraph("kubectl ran.")]);
        assert_eq!((asked(&first_asked), asked(&second_asked)), (0, 1));
    }
}
