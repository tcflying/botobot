//! Shared output limiting and artifact spill helpers.

use crate::artifact::ArtifactStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSnapshot {
    pub text: String,
    pub total_bytes: usize,
    pub total_lines: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillSnapshot {
    pub inline_text: String,
    pub artifact_uri: Option<String>,
    pub total_bytes: usize,
    pub total_lines: usize,
    pub truncated: bool,
}

/// Keep the tail of `text` with UTF-8 safe boundaries and exact total counts.
pub fn tail_snapshot(text: &str, max_bytes: usize) -> OutputSnapshot {
    let total_bytes = text.len();
    let total_lines = count_lines(text);
    if text.len() <= max_bytes {
        return OutputSnapshot {
            text: text.to_string(),
            total_bytes,
            total_lines,
            truncated: false,
        };
    }

    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    OutputSnapshot {
        text: format!(
            "[tail {} of {} bytes, {} lines]\n{}",
            text.len() - start,
            total_bytes,
            total_lines,
            &text[start..]
        ),
        total_bytes,
        total_lines,
        truncated: true,
    }
}

/// Spill large text to an artifact and leave a bounded tail preview inline.
pub fn spill_or_inline(
    store: &ArtifactStore,
    text: &str,
    max_inline_bytes: usize,
    tail_bytes: usize,
) -> std::io::Result<SpillSnapshot> {
    let total_bytes = text.len();
    let total_lines = count_lines(text);
    if total_bytes <= max_inline_bytes {
        return Ok(SpillSnapshot {
            inline_text: text.to_string(),
            artifact_uri: None,
            total_bytes,
            total_lines,
            truncated: false,
        });
    }

    let id = store.put_text(text)?;
    let tail = tail_snapshot(text, tail_bytes);
    Ok(SpillSnapshot {
        inline_text: format!(
            "[output spilled to artifact://{id}; total {} bytes, {} lines]\n{}",
            total_bytes, total_lines, tail.text
        ),
        artifact_uri: Some(format!("artifact://{id}")),
        total_bytes,
        total_lines,
        truncated: true,
    })
}

pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactStore;

    #[test]
    fn tail_snapshot_is_utf8_safe_and_counts_totals() {
        let text = "a\nβeta\n最後\n";
        let snap = tail_snapshot(text, 7);
        assert!(snap.truncated);
        assert_eq!(snap.total_bytes, text.len());
        assert_eq!(snap.total_lines, 3);
        assert!(snap.text.contains("bytes"));
        assert!(std::str::from_utf8(snap.text.as_bytes()).is_ok());
    }

    #[test]
    fn spill_or_inline_writes_artifact_with_tail_preview() {
        let dir = std::env::temp_dir().join(format!("botobot-output-{}", uuid::Uuid::new_v4()));
        let store = ArtifactStore::new(&dir).unwrap();
        let text = "line\n".repeat(20);
        let snap = spill_or_inline(&store, &text, 10, 12).unwrap();

        assert!(snap.truncated);
        let uri = snap.artifact_uri.expect("large output should spill");
        let id = uri.strip_prefix("artifact://").unwrap();
        assert_eq!(store.get_text(id).unwrap(), text);
        assert!(snap.inline_text.contains("artifact://"));
        assert!(snap.inline_text.contains("tail"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
