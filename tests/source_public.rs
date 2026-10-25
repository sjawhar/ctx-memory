use std::collections::BTreeMap;

use anyhow::Result;
use ctx_memory::source::{gmail, slack};
use serde_json::json;

#[test]
fn public_record_converters_are_available_to_downstream_crates() -> Result<()> {
    let names = BTreeMap::from([("U0EXAMPLE9".to_owned(), "Alice".to_owned())]);
    let slack_record = slack::record_from_message(
        &json!({
            "ts": "1792951800.000100",
            "user": "U0EXAMPLE9",
            "text": "hello",
        }),
        "C0EXAMPLE01",
        "#general",
        &names,
        "acme",
    )?;
    let gmail_record = gmail::record_from_message(&json!({
        "id": "0123456789abcdef",
        "threadId": "fedcba9876543210",
        "internalDate": "1792951800000",
        "payload": {
            "headers": [
                {"name": "From", "value": "alice@example.com"},
                {"name": "Subject", "value": "Example"}
            ]
        }
    }))?;

    assert_eq!(slack_record.pointer, "slack:C0EXAMPLE01/1792951800.000100");
    assert_eq!(slack_record.author.as_deref(), Some("Alice"));
    assert_eq!(gmail_record.pointer, "gmail:0123456789abcdef");
    assert_eq!(gmail_record.title.as_deref(), Some("Example"));
    Ok(())
}
