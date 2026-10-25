//! Human-readable rendering for records captured in a fetch window.

use crate::record;
use crate::windows::SourcedRecord;

/// Renders window records grouped by their configured source.
#[must_use]
pub fn window(records: &[SourcedRecord]) -> String {
    let mut rendered = String::new();
    let mut source: Option<&str> = None;
    for sourced in records {
        if source != Some(sourced.source.as_str()) {
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str("# ");
            rendered.push_str(&sourced.source);
            rendered.push_str("\n\n");
            source = Some(&sourced.source);
        }
        rendered.push_str(&record::render(&sourced.record));
        rendered.push_str("\n\n");
    }
    rendered
}
