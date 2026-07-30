//! Ticket 59 (eval-llm-judge-scoring): LLM-judge rubric scoring. A judge
//! score is a TRACKED METRIC only -- it never gates a case's pass/fail
//! outcome (ticket 58's deterministic assertions are the only thing that
//! does that; see `lib.rs`'s per-case loop, which routes
//! `case::Assertion::JudgeRubric` here instead of through
//! `assertions::check_assertion`, folding the result into
//! `CaseOutcome::judge_scores` -- a field entirely separate from
//! `assertion_outcomes`/`passed`).
//!
//! Deliberately a direct, minimal OpenAI-compatible chat-completions call
//! (`reqwest`, the SAME `ROKR_OPENAI_BASE_URL`/`ROKR_OPENAI_MODEL`/
//! `ROKR_OPENAI_API_KEY` env vars `rokr_app::headless::run_result_object`'s
//! own provider resolves from -- see `crates/rokr-provider/src/openai.rs`'s
//! `ENV_*` consts, duplicated here as literals rather than adding a new
//! `rokr-provider` dependency edge) rather than reusing `rokr-provider`'s
//! full `Provider` trait + `rokr-core::Message`/`ToolSpec` machinery built
//! for the agentic tool-loop -- a single non-agentic scoring call needs
//! none of that, and pulling those two crates in would widen `rokr-eval`'s
//! dependency surface past `lib.rs`'s documented "depends only on
//! rokr-app" boundary for no behavioral gain.

use serde::{Deserialize, Serialize};

/// One judge-rubric assertion's scoring result. Deliberately has NO
/// `passed` field -- see this module's doc comment.
#[derive(Debug, Clone)]
pub struct JudgeScore {
    /// A short, stable label identifying which rubric this is, mirroring
    /// `assertions::AssertionOutcome::description`'s shape.
    pub description: String,
    /// 0.0-1.0, as returned by the judge model's own JSON verdict.
    pub score: f64,
    pub detail: String,
}

const ENV_BASE_URL: &str = "ROKR_OPENAI_BASE_URL";
const ENV_MODEL: &str = "ROKR_OPENAI_MODEL";
const ENV_API_KEY: &str = "ROKR_OPENAI_API_KEY";

#[derive(Debug, Serialize)]
struct JudgeRequest {
    model: String,
    messages: Vec<JudgeWireMessage>,
    temperature: f64,
}

#[derive(Debug, Serialize)]
struct JudgeWireMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct JudgeResponse {
    choices: Vec<JudgeChoice>,
}

#[derive(Debug, Deserialize)]
struct JudgeChoice {
    message: JudgeChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct JudgeChoiceMessage {
    content: String,
}

/// The judge model's own expected reply shape, parsed out of the wire
/// response's `choices[0].message.content` text (a scripted/mocked judge in
/// tests, a real judge model's own JSON reply in production).
#[derive(Debug, Deserialize)]
struct JudgeVerdict {
    score: f64,
}

/// Scores `transcript` (a case's headless-run final result text) against
/// `rubric` by sending both to a judge model over the configured
/// OpenAI-compatible endpoint, at a low (`0.0`) temperature to reduce (not
/// eliminate) verdict variance, per this ticket's `## Context`. Returns
/// `Err` on any missing env var, transport failure, non-2xx response, or a
/// reply that doesn't parse as the expected `{"score": <f64>}` verdict --
/// callers (`lib.rs`'s per-case loop) treat a scoring failure as "no score
/// recorded for this assertion", never as a case run/pass-fail failure.
pub async fn score_rubric(transcript: &str, rubric: &str) -> Result<JudgeScore, String> {
    let base_url =
        std::env::var(ENV_BASE_URL).map_err(|_| format!("missing env var {ENV_BASE_URL}"))?;
    let model = std::env::var(ENV_MODEL).map_err(|_| format!("missing env var {ENV_MODEL}"))?;
    let api_key =
        std::env::var(ENV_API_KEY).map_err(|_| format!("missing env var {ENV_API_KEY}"))?;

    let prompt = format!(
        "You are grading an AI agent's transcript against a rubric. Respond with ONLY a JSON \
         object of the form {{\"score\": <float 0.0-1.0>}}, no other text.\n\nRubric:\n{rubric}\n\n\
         Transcript:\n{transcript}"
    );

    let request_body = JudgeRequest {
        model,
        messages: vec![JudgeWireMessage {
            role: "user".to_string(),
            content: prompt,
        }],
        temperature: 0.0,
    };

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .bearer_auth(&api_key)
        .json(&request_body)
        .send()
        .await
        .map_err(|err| format!("judge request failed: {err}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("failed to read judge response body: {err}"))?;
    if !status.is_success() {
        return Err(format!("judge returned status {status}: {body}"));
    }

    let parsed: JudgeResponse = serde_json::from_str(&body).map_err(|err| {
        format!("failed to parse judge response envelope: {err}; body was: {body}")
    })?;
    let content = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "judge response had no choices".to_string())?
        .message
        .content;
    let verdict: JudgeVerdict = serde_json::from_str(content.trim()).map_err(|err| {
        format!("failed to parse judge verdict JSON: {err}; content was: {content:?}")
    })?;

    Ok(JudgeScore {
        description: format!("judge_rubric({rubric:?})"),
        score: verdict.score,
        detail: format!("judge scored {}", verdict.score),
    })
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `score_rubric` must call the configured judge endpoint and return the
    /// numeric score parsed out of a scripted/mocked verdict reply --
    /// proving the judge CLIENT path end to end. `JudgeScore` deliberately
    /// carries no `passed` field at all (see this module's eventual doc
    /// comment) -- the "recorded separately from pass/fail" guarantee is
    /// enforced at the type level here and at the case-runner level in the
    /// acceptance test in `tests/eval_test.rs`.
    #[tokio::test]
    async fn judge_rubric_assertion_score_recorded_separately_from_pass_fail_outcome() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "{\"score\": 0.75}"}}]
            })))
            .mount(&mock)
            .await;

        // Safety: this unit test is the only test in judge.rs's own test
        // module touching process env vars, and cargo runs the lib crate's
        // unit-test binary as a separate process from tests/eval_test.rs's
        // integration-test binary, so there is no cross-test race here.
        unsafe {
            std::env::set_var(super::ENV_BASE_URL, mock.uri());
            std::env::set_var(super::ENV_MODEL, "gpt-4o-mini");
            std::env::set_var(super::ENV_API_KEY, "test-key");
        }

        let result = super::score_rubric("agent transcript text", "did the agent do the task well?")
            .await
            .expect("expected score_rubric to succeed against the mocked judge endpoint");

        assert_eq!(
            result.score, 0.75,
            "expected the score parsed from the scripted judge reply, got: {result:?}"
        );
    }
}
