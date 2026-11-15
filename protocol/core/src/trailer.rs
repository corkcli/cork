//! The commit trailer that ties a git commit to the ops it carries:
//!
//! ```text
//! Cork-Ops: a1.17,a1.18,b2.3
//! ```
//!
//! One or more `Cork-Ops:` lines in the message's final paragraph; ids are
//! comma-separated (spaces around commas are allowed). An agent's tested push
//! writes the trailer and sends `promote` for the same ids. Commits from
//! before the rename to Cork carry `Collide-Ops:`, read the same.

pub const TRAILER_KEY: &str = "Cork-Ops";
/// The key before the rename: read, never written.
pub const OLD_TRAILER_KEY: &str = "Collide-Ops";

/// `Cork-Ops: a,b,c`, wrapped onto several lines past ~900 bytes.
pub fn format_trailer<S: AsRef<str>>(op_ids: &[S]) -> String {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for id in op_ids {
        let id = id.as_ref();
        if !cur.is_empty() && cur.len() + id.len() > 900 {
            lines.push(format!("{TRAILER_KEY}: {cur}"));
            cur.clear();
        }
        if !cur.is_empty() {
            cur.push(',');
        }
        cur.push_str(id);
    }
    if !cur.is_empty() {
        lines.push(format!("{TRAILER_KEY}: {cur}"));
    }
    lines.join("\n")
}

/// Every op id in the message's trailer block, in order, without duplicates.
pub fn parse_trailer(message: &str) -> Vec<String> {
    let trimmed = message.trim_end();
    let block = trimmed.rsplit("\n\n").next().unwrap_or("");
    let mut ids: Vec<String> = Vec::new();
    for line in block.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim();
        if !k.eq_ignore_ascii_case(TRAILER_KEY) && !k.eq_ignore_ascii_case(OLD_TRAILER_KEY) {
            continue;
        }
        for id in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// Add the trailer to a commit message, joining an existing trailer block
/// (lines like `Co-Authored-By:`) when there is one.
pub fn append_trailer<S: AsRef<str>>(message: &str, op_ids: &[S]) -> String {
    if op_ids.is_empty() {
        return message.to_string();
    }
    let msg = message.trim_end();
    let last = msg.rsplit("\n\n").next().unwrap_or("");
    let is_trailers = msg.contains("\n\n")
        && !last.is_empty()
        && last.lines().all(|l| l.split_once(':').is_some_and(|(k, _)| !k.is_empty() && !k.contains(' ')));
    let sep = if is_trailers { "\n" } else { "\n\n" };
    format!("{msg}{sep}{}\n", format_trailer(op_ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let msg = append_trailer("Fix the parser\n\nLonger body.", &["a.1", "b.2"]);
        assert_eq!(msg, "Fix the parser\n\nLonger body.\n\nCork-Ops: a.1,b.2\n");
        assert_eq!(parse_trailer(&msg), vec!["a.1", "b.2"]);
    }

    #[test]
    fn joins_an_existing_trailer_block_and_tolerates_spaces() {
        let msg = append_trailer("Subject\n\nCo-Authored-By: X <x@y>", &["a.1"]);
        assert!(msg.ends_with("Co-Authored-By: X <x@y>\nCork-Ops: a.1\n"));
        let ids = parse_trailer("S\n\nCollide-Ops: a.1 , b.2\ncollide-ops: b.2,c.3\n");
        assert_eq!(ids, vec!["a.1", "b.2", "c.3"]);
    }

    #[test]
    fn only_the_final_paragraph_counts() {
        assert!(parse_trailer("S\n\nCork-Ops: a.1\n\nbody after").is_empty());
    }

    #[test]
    fn wraps_long_lists() {
        let ids: Vec<String> = (0..200).map(|i| format!("session-{i}.{i}")).collect();
        let t = format_trailer(&ids);
        assert!(t.lines().count() > 1);
        assert!(t.lines().all(|l| l.len() < 1000));
        assert_eq!(parse_trailer(&format!("S\n\n{t}")), ids);
    }
}
