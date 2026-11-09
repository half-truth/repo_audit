//! Position-driven parsing of `git log -z --raw --numstat`.
//!
//! Token layout:
//! ```text
//! <\x1eheader\x1d>  [\n:raw-meta path(1 or 2)]*  [a\td\tpath | a\td\t + old + new]*
//! ```
//! Path tokens are consumed **by position, never by pattern-matching the path**.
//! File names may contain spaces, quotes, CJK, newlines, or any byte at all —
//! that is precisely why this tool is not a shell script.

use super::record::{dec, CommitRecord, FileChange, BODY_LIMIT};
use crate::error::{AuditError, Result};
use crate::util::text::py_strip;
use std::io::Read;

/// Split a NUL-delimited byte stream into tokens, buffering the trailing
/// partial token across reads and flushing it at EOF (git's last token has no
/// terminator, and dropping it loses the final commit's last file).
///
/// A read error is **not** an EOF: it is stored and must be collected with
/// [`Self::take_err`] by whoever consumes the stream. Silently treating it as
/// a short history is the one failure an audit tool may not have.
pub struct NulTokens<R: Read> {
    reader: R,
    rem: Vec<u8>,
    err: Option<std::io::Error>,
}

impl<R: Read> NulTokens<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            rem: Vec::new(),
            err: None,
        }
    }

    /// The first read error encountered, if any. The iterator itself cannot
    /// carry an error (its `Item` is a bare `Vec<u8>`), so the error waits here.
    pub fn take_err(&mut self) -> Option<std::io::Error> {
        self.err.take()
    }
}

impl<R: Read> Iterator for NulTokens<R> {
    type Item = Vec<u8>;
    fn next(&mut self) -> Option<Vec<u8>> {
        let mut buf = [0u8; 1 << 16];
        loop {
            let mut data = std::mem::take(&mut self.rem);
            // Split on an already-buffered NUL *before* reading more. Checking
            // only after a read loses everything past the first token once the
            // stream ends, because the read then returns EOF with the rest
            // still unparsed.
            if let Some(i) = data.iter().position(|&b| b == 0) {
                self.rem = data[i + 1..].to_vec();
                return Some(data[..i].to_vec());
            }
            match self.reader.read(&mut buf) {
                Ok(0) => {
                    // EOF: emit whatever is left, then stop for good.
                    self.rem.clear();
                    return if data.is_empty() { None } else { Some(data) };
                }
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    self.rem = data;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // The buffered partial token belongs to a broken stream; the
                    // consumer fails on `take_err` anyway.
                    self.rem.clear();
                    self.err = Some(e);
                    return None;
                }
            }
        }
    }
}

fn parse_int(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    std::str::from_utf8(b).ok()?.parse::<i64>().ok()
}

/// One `--numstat` line: added, deleted, and the binary marker.
///
/// `old`/`new` hold the `R100`-style rename/copy suffix when the line carries
/// one, and are `None` for a binary file — which is how a NUL blob is detected
/// without a second pass over the blob.
type Numstat = (Option<i64>, Option<i64>, Option<String>, String);

/// Parse a whole token stream into commits. `warnings` collects recoverable
/// oddities (raw/numstat count mismatch) that are surfaced rather than hidden.
pub fn parse_log_tokens(
    tokens: impl IntoIterator<Item = Vec<u8>>,
    warnings: &mut Vec<String>,
) -> Result<Vec<CommitRecord>> {
    let mut it = tokens.into_iter().peekable();
    let mut out = Vec::new();

    // `take()` in Python raised ParseError on exhaustion; here the iterator is
    // exhausted exactly where Python's `next()` raised StopIteration.
    let mut tok = it.next();
    while let Some(t) = tok {
        if !t.starts_with(b"\x1e") {
            if t.is_empty() {
                tok = it.next();
                continue;
            }
            return Err(AuditError::with_hint(
                "Failed to parse git log output (expected a commit header)",
                "This may be a git version difference; include your git --version output when reporting it",
            ));
        }
        // header = everything after \x1e, up to the first \x1d
        let rest = &t[1..];
        let dpos = rest.iter().position(|&b| b == 0x1d);
        let Some(dpos) = dpos else {
            return Err(AuditError::new("Incorrect number of commit header fields"));
        };
        let head = &rest[..dpos];
        let f: Vec<&[u8]> = splitn(head, 0x1f, 8);
        if f.len() != 8 {
            return Err(AuditError::new("Incorrect number of commit header fields"));
        }
        let mut rec = CommitRecord {
            sha: String::from_utf8_lossy(f[0]).into_owned(),
            parents: String::from_utf8_lossy(f[1])
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            author_name: dec(f[2]),
            author_email: dec(f[3]),
            author_ts: parse_int(f[4]).unwrap_or(0),
            commit_ts: parse_int(f[5]).unwrap_or(0),
            subject: dec(f[6]),
            body: {
                let b = py_strip(&dec(f[7]));
                b.chars().take(BODY_LIMIT).collect()
            },
            files: Vec::new(),
        };

        // Per-commit: the raw block then the numstat block, both until the
        // next \x1e header.
        let mut raws: Vec<String> = Vec::new();
        let mut nums: Vec<Numstat> = Vec::new();
        let mut t = it.next();
        while let Some(tt) = t.as_ref() {
            if tt.starts_with(b"\x1e") {
                break;
            }
            let stripped: &[u8] = {
                let mut i = 0;
                while i < tt.len() && tt[i] == b'\n' {
                    i += 1;
                }
                &tt[i..]
            };
            if stripped.first() == Some(&b':') {
                // ":100644 100644 <sha> <sha> <status>"
                let last = rsplit_last(stripped, b' ');
                let status = match last {
                    Some(s) if !s.is_empty() => dec(s).chars().take(1).collect::<String>(),
                    _ => "M".to_string(),
                };
                let paths_to_skip = if status == "R" || status == "C" { 2 } else { 1 };
                for _ in 0..paths_to_skip {
                    if it.next().is_none() {
                        return Err(AuditError::with_hint(
                            "git log output was truncated",
                            "Try again; use --reindex if the problem persists",
                        ));
                    }
                }
                raws.push(status);
            } else if !stripped.is_empty() {
                let parts = splitn(stripped, b'\t', 3);
                if parts.len() != 3 {
                    return Err(AuditError::new("Malformed numstat line"));
                }
                let ai = if parts[0] == b"-" {
                    None
                } else {
                    parse_int(parts[0])
                };
                let di = if parts[1] == b"-" {
                    None
                } else {
                    parse_int(parts[1])
                };
                if parts[2].is_empty() {
                    // Rename/copy: the path was split into its own two tokens
                    // because it contained a NUL-adjacent separator.
                    let old = it.next();
                    let new = it.next();
                    let (Some(old), Some(new)) = (old, new) else {
                        return Err(AuditError::with_hint(
                            "git log output was truncated",
                            "Try again; use --reindex if the problem persists",
                        ));
                    };
                    nums.push((ai, di, Some(dec(&old)), dec(&new)));
                } else {
                    nums.push((ai, di, None, dec(parts[2])));
                }
            }
            t = it.next();
        }

        if raws.len() != nums.len() {
            warnings.push(format!(
                "{}: raw/numstat entry count mismatch ({}/{}); treating status as M",
                rec.sha.chars().take(8).collect::<String>(),
                raws.len(),
                nums.len()
            ));
            raws = nums
                .iter()
                .map(|n| if n.2.is_some() { "R" } else { "M" }.to_string())
                .collect();
        }
        for (st, (a, d, old, new)) in raws.into_iter().zip(nums) {
            rec.files.push(FileChange {
                status: st,
                path: new,
                old_path: old,
                added: a.unwrap_or(0),
                deleted: d.unwrap_or(0),
                is_binary: a.is_none(),
            });
        }
        out.push(rec);
        tok = t;
    }
    Ok(out)
}

/// `bytes.split(sep, maxsplit)` — at most `maxsplit` splits, so the final
/// element keeps every remaining separator.
fn splitn(hay: &[u8], sep: u8, maxsplit: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut splits = 0usize;
    while splits < maxsplit {
        match hay[start..].iter().position(|&b| b == sep) {
            Some(i) => {
                out.push(&hay[start..start + i]);
                start += i + 1;
                splits += 1;
            }
            None => break,
        }
    }
    out.push(&hay[start..]);
    out
}

/// `bytes.rsplit(sep, 1)[-1]` — the part after the last occurrence.
fn rsplit_last(hay: &[u8], sep: u8) -> Option<&[u8]> {
    hay.iter().rposition(|&b| b == sep).map(|i| &hay[i + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(v: &[&[u8]]) -> Vec<Vec<u8>> {
        v.iter().map(|s| s.to_vec()).collect()
    }

    #[test]
    fn tricky_paths_and_rename() {
        let h = b"\x1e".to_vec();
        let mut head = h;
        head.extend(b"a".repeat(40));
        head.extend(b"\x1f\x1fN\x1fe@x\x1f1\x1f2\x1fsubj\x1fbody\n\x1d");
        let input = toks(&[
            &head,
            b"\n:100644 100644 1111111 2222222 R090",
            b"old\ttab.txt",
            b"new name.txt",
            b":000000 100644 0000000 3333333 A",
            // A path that *starts* with \x1e must not be mistaken for a header.
            b"\x1ehdr-like.txt",
            b"3\t1\t",
            b"old\ttab.txt",
            b"new name.txt",
            b"-\t-\t\x1ehdr-like.txt",
            b"",
        ]);
        let mut w = Vec::new();
        let recs = parse_log_tokens(input, &mut w).unwrap();
        assert_eq!(recs.len(), 1);
        let f = &recs[0].files;
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].status, "R");
        assert_eq!(f[0].old_path.as_deref(), Some("old\ttab.txt"));
        assert_eq!(f[0].path, "new name.txt");
        assert_eq!(f[0].added, 3);
        assert!(f[1].is_binary);
        assert_eq!(f[1].path, "\u{1e}hdr-like.txt");
        assert!(w.is_empty(), "unexpected warnings: {w:?}");
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let mut head = b"\x1e".to_vec();
        head.extend(b"a".repeat(40));
        head.extend(b"\x1f\x1fN\x1fe@x\x1f1\x1f2\x1fs\x1fb\x1d");
        let input = toks(&[&head, b":1 1 a b R100", b"only-one"]);
        let mut w = Vec::new();
        assert!(parse_log_tokens(input, &mut w).is_err());
    }

    #[test]
    fn nul_splitting_reassembles_across_chunks() {
        let data = b"aa\0bb\0ccc\0".to_vec();
        let toks: Vec<Vec<u8>> = NulTokens::new(&data[..]).collect();
        assert_eq!(toks, vec![b"aa".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]);
    }

    #[test]
    fn trailing_token_without_nul_is_still_emitted() {
        // git's final token is unterminated; dropping it loses a real file entry.
        let data = b"aa\0bb".to_vec();
        let toks: Vec<Vec<u8>> = NulTokens::new(&data[..]).collect();
        assert_eq!(toks, vec![b"aa".to_vec(), b"bb".to_vec()]);
    }

    #[test]
    fn empty_stream_yields_nothing() {
        let toks: Vec<Vec<u8>> = NulTokens::new(&b""[..]).collect();
        assert!(toks.is_empty());
    }

    #[test]
    fn splitn_keeps_trailing_separators_in_the_last_field() {
        assert_eq!(
            splitn(b"a\x1fb\x1fc", 0x1f, 7),
            vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]
        );
        // The body field legitimately contains \x1f? No — but \n yes, and this
        // is why the split count is pinned at 7 rather than "all".
        assert_eq!(splitn(b"a\x1fb\x1fc\x1fd", 0x1f, 2).len(), 3);
    }

    #[test]
    fn body_is_stripped_and_capped() {
        let long = "x".repeat(BODY_LIMIT + 500);
        let mut head = b"\x1e".to_vec();
        head.extend(b"a".repeat(40));
        head.extend(format!("\x1f\x1fN\x1fe@x\x1f1\x1f2\x1fs\x1f  {long}  \x1d").into_bytes());
        let mut w = Vec::new();
        let recs = parse_log_tokens(vec![head], &mut w).unwrap();
        assert_eq!(recs[0].body.chars().count(), BODY_LIMIT);
    }

    #[test]
    fn raw_numstat_mismatch_warns_and_falls_back() {
        let mut head = b"\x1e".to_vec();
        head.extend(b"b".repeat(40));
        head.extend(b"\x1f\x1fN\x1fe@x\x1f1\x1f2\x1fs\x1fb\x1d");
        // One numstat line, no matching raw line.
        let input = toks(&[&head, b"1\t1\tsolo.txt", b""]);
        let mut w = Vec::new();
        let recs = parse_log_tokens(input, &mut w).unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("raw/numstat"));
        assert_eq!(recs[0].files[0].status, "M");
    }
}
