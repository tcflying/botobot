//! 跨分片 `<think>...</think>` 检测器：内部内容标 Reasoning、外部标 Text。
//!
//! 跨 SSE 分片时，标签可能被切断——所以每次 `feed` 都把末尾 `START.len()-1` / `END.len()-1`
//! 字节保留在 `buf` 里，留到下一片再判定。

#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Reasoning(String),
}

#[derive(Default)]
pub struct ThinkSplitter {
    inside: bool,
    buf: String,
}

impl ThinkSplitter {
    /// 推入一段文本，返回已可确定归属的片段；不确定的尾部留在内部 buf。
    pub fn feed(&mut self, text: &str) -> Vec<Piece> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        loop {
            if self.inside {
                const END: &str = "</think>";
                if let Some(pos) = self.buf.find(END) {
                    let pre: String = self.buf[..pos].to_string();
                    self.buf = self.buf[pos + END.len()..].to_string();
                    self.inside = false;
                    if !pre.is_empty() {
                        out.push(Piece::Reasoning(pre));
                    }
                } else {
                    let (safe, keep) = split_keep_tail(&self.buf, END.len() - 1);
                    if !safe.is_empty() {
                        out.push(Piece::Reasoning(safe.to_string()));
                    }
                    self.buf = keep.to_string();
                    break;
                }
            } else {
                const START: &str = "<think>";
                if let Some(pos) = self.buf.find(START) {
                    let pre: String = self.buf[..pos].to_string();
                    self.buf = self.buf[pos + START.len()..].to_string();
                    self.inside = true;
                    if !pre.is_empty() {
                        out.push(Piece::Text(pre));
                    }
                } else {
                    let (safe, keep) = split_keep_tail(&self.buf, START.len() - 1);
                    if !safe.is_empty() {
                        out.push(Piece::Text(safe.to_string()));
                    }
                    self.buf = keep.to_string();
                    break;
                }
            }
        }
        out
    }

    /// 流结束时把残留 buf 全部吐出（视为 outside 段=Text）。
    pub fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

/// 保留末尾 `keep` 字节用于跨分片标签检测，其余安全切出。
fn split_keep_tail(buf: &str, keep: usize) -> (&str, &str) {
    if buf.len() <= keep {
        return ("", buf);
    }
    let mut idx = buf.len() - keep;
    while idx > 0 && !buf.is_char_boundary(idx) {
        idx -= 1;
    }
    buf.split_at(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_in_single_chunk() {
        let mut s = ThinkSplitter::default();
        let out = s.feed("a<think>b</think>c");
        // 末段 Text("c") 在 feed 时不确定(无标签触发),留在 buf,flush 时吐出。
        assert_eq!(
            out,
            vec![Piece::Text("a".into()), Piece::Reasoning("b".into())]
        );
        assert_eq!(s.flush(), Some("c".into()));
    }

    #[test]
    fn tags_split_across_chunks() {
        let mut s = ThinkSplitter::default();
        let out1 = s.feed("<think>rea");
        assert!(out1.is_empty(), "首片不确定归属");
        let out2 = s.feed("son</think>an");
        // 尾段 Text("an") 同上,留到 flush。
        assert_eq!(out2, vec![Piece::Reasoning("reason".into())]);
        assert_eq!(s.flush(), Some("an".into()));
    }

    #[test]
    fn unclosed_think_flushes_as_text() {
        let mut s = ThinkSplitter::default();
        s.feed("<think>forever");
        let rest = s.flush().unwrap();
        assert_eq!(rest, "forever", "未闭合的 think 段内容在 flush 时吐出");
    }
}
