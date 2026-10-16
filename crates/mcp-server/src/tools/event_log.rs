use crate::state::SharedState;
use crate::tools::response::{structured, trim_items_to_budget};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;

/// Owned filter set forwarded from the `event_log` tool. All dimensions are optional; the
/// 1C side treats an absent filter as "no restriction". `contains`/`metadata` are best-effort
/// (see [`event_log`] docs) — the platform's `Отбор` structure does not cover a free-text
/// substring, so the extension applies `contains` as a post-read row filter.
#[derive(Debug, Default)]
pub struct EventLogQuery {
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub level: Option<String>,
    pub user: Option<String>,
    pub event: Option<String>,
    pub metadata: Option<String>,
    pub contains: Option<String>,
    pub limit: Option<u32>,
    pub connection: Option<String>,
}

fn build_request(query: EventLogQuery) -> onec_client::EventLogRequest {
    onec_client::EventLogRequest {
        date_from: query.date_from,
        date_to: query.date_to,
        level: query.level,
        user: query.user,
        event: query.event,
        metadata: query.metadata,
        contains: query.contains,
        limit: query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT),
    }
}

/// Reads the 1C event log (журнал регистрации) via the `BSL_Analyzer` extension.
///
/// Records come back newest-first, capped by `limit`. `contains` is a case-insensitive
/// substring filter the extension applies to the comment/data columns after the read, since
/// the platform's `Отбор` structure has no free-text dimension. Requires `--onec-url` and a
/// connecting user that holds the `EventLog` right.
pub async fn event_log(
    state: &SharedState,
    query: EventLogQuery,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected = state
        .onec_connection(query.connection.as_deref())
        .map_err(|e| McpError::invalid_params(e, None))?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;

    let request = build_request(query);

    let result = client.event_log(&request).await.map_err(|e| {
        McpError::internal_error(format!("Ошибка чтения журнала регистрации в 1С: {e}"), None)
    })?;

    Ok(structured(event_log_body(result, max_output_tokens)))
}

/// Shape the platform's answer into the response body, bounding it by the output budget on
/// top of the record-count `limit`. Records arrive newest-first, so a budget trim drops the
/// oldest tail; `total` stays the honest count the platform reported and `returned` says how
/// many records actually survived into `rows`.
fn event_log_body(result: onec_client::EventLogResult, max_output_tokens: usize) -> Value {
    let mut rows: Vec<Value> = result.rows.into_iter().map(Value::Array).collect();
    let read = rows.len();
    let budget_exhausted = trim_items_to_budget(&mut rows, max_output_tokens);
    // `trim_items_to_budget` also flags a single kept record that is itself over budget, where
    // nothing was actually dropped. `truncated` means "records are missing", so it follows the
    // real drop, while `budget_exhausted` keeps meaning "the response exceeds the budget".
    let dropped = rows.len() < read;

    let mut body = json!({
        "columns": result.columns,
        "returned": rows.len(),
        "rows": rows,
        "total": result.total,
        "truncated": result.truncated || dropped,
    });
    if budget_exhausted {
        body["budget_exhausted"] = json!(true);
        // When the record cap also fired, say so: raising `max_output_tokens` alone stops at
        // `limit`, so the agent must lift both to reach deeper records.
        body["budget_hint"] = json!(if !dropped {
            "the single returned record is itself larger than max_output_tokens; nothing was dropped, but the response exceeds the budget"
        } else if result.truncated {
            "records truncated by both max_output_tokens and the `limit` cap; narrow the filters (date range/level/event/metadata), or raise BOTH max_output_tokens and limit"
        } else {
            "records truncated to fit max_output_tokens; narrow the filters (date range/level/event/metadata) or raise the budget"
        });
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_defaults_and_clamps() {
        assert_eq!(build_request(EventLogQuery::default()).limit, DEFAULT_LIMIT);
        assert_eq!(build_request(EventLogQuery { limit: Some(5), ..Default::default() }).limit, 5);
        assert_eq!(
            build_request(EventLogQuery { limit: Some(99_999), ..Default::default() }).limit,
            MAX_LIMIT
        );
    }

    #[test]
    fn filters_pass_through() {
        let req = build_request(EventLogQuery {
            level: Some("Ошибка".into()),
            user: Some("Администратор".into()),
            contains: Some("оштрафован".into()),
            ..Default::default()
        });
        assert_eq!(req.level.as_deref(), Some("Ошибка"));
        assert_eq!(req.user.as_deref(), Some("Администратор"));
        assert_eq!(req.contains.as_deref(), Some("оштрафован"));
    }

    #[test]
    fn omitted_filters_are_not_serialized() {
        let req = build_request(EventLogQuery {
            level: Some("Ошибка".into()),
            limit: Some(10),
            ..Default::default()
        });
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body["level"], "Ошибка");
        assert_eq!(body["limit"], 10);
        assert!(body.get("date_from").is_none(), "absent filter must be omitted: {body}");
        assert!(body.get("user").is_none());
    }

    #[tokio::test]
    async fn errors_without_onec_client() {
        let state = SharedState::shared();
        let result = event_log(&state, EventLogQuery::default(), 6000).await;
        assert!(result.is_err(), "should fail without onec client");
    }

    fn sample_result(rows: usize, truncated: bool) -> onec_client::EventLogResult {
        onec_client::EventLogResult {
            columns: vec!["Дата".into(), "Событие".into(), "Комментарий".into()],
            rows: (0..rows)
                .map(|i| {
                    vec![
                        json!(format!("2026-07-25T00:00:{i:02}")),
                        json!("_$Data$_.Post"),
                        json!("x".repeat(200)),
                    ]
                })
                .collect(),
            total: rows as u32,
            truncated,
        }
    }

    #[test]
    fn body_keeps_every_record_within_budget() {
        let body = event_log_body(sample_result(3, false), 6000);
        assert_eq!(body["rows"].as_array().unwrap().len(), 3);
        assert_eq!(body["returned"], 3);
        assert_eq!(body["truncated"], false);
        assert!(body.get("budget_exhausted").is_none(), "nothing to flag: {body}");
    }

    #[test]
    fn body_trims_over_budget_records_and_flags_them() {
        let body = event_log_body(sample_result(100, false), 100);
        let rows = body["rows"].as_array().unwrap();
        assert!(rows.len() < 100, "over-budget records must be dropped: {}", rows.len());
        assert_eq!(body["returned"], rows.len());
        assert_eq!(body["total"], 100, "total stays the honest platform count");
        assert_eq!(body["truncated"], true);
        assert_eq!(body["budget_exhausted"], true);
        let hint = body["budget_hint"].as_str().unwrap();
        assert!(!hint.contains("BOTH"), "the limit cap did not fire: {hint}");
    }

    #[test]
    fn one_oversized_record_is_delivered_whole_without_claiming_records_are_missing() {
        let body = event_log_body(sample_result(1, false), 1);
        assert_eq!(body["returned"], 1, "the only record is still delivered");
        assert_eq!(body["truncated"], false, "nothing was dropped: {body}");
        assert_eq!(body["budget_exhausted"], true, "but the response does exceed the budget");
        let hint = body["budget_hint"].as_str().unwrap();
        assert!(hint.contains("nothing was dropped"), "hint must not claim a loss: {hint}");
    }

    #[test]
    fn body_says_raising_the_budget_alone_will_not_help_when_limit_also_capped() {
        let body = event_log_body(sample_result(100, true), 100);
        let hint = body["budget_hint"].as_str().unwrap();
        assert!(hint.contains("BOTH max_output_tokens and limit"), "hint must name both: {hint}");
    }
}
