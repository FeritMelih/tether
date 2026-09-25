//! The terminal queries the host answers itself. A program asks its terminal where the cursor
//! is and what it is; with no window attached nobody would answer, and with two attached both
//! would, one of them at the wrong size. So these few are answered from the screen model and
//! taken out of the output passed on. Output arrives in chunks that can split a query
//! anywhere, so a tail that may still become one is held back for the next chunk.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query {
    /// `ESC[6n`: cursor position.
    CursorPosition,
    /// `ESC[5n`: device status.
    Status,
    /// `ESC[c`, `ESC[0c`: primary device attributes.
    PrimaryAttributes,
    /// `ESC[>c`, `ESC[>0c`: secondary device attributes.
    SecondaryAttributes,
}

const PATTERNS: &[(&[u8], Query)] = &[
    (b"\x1b[6n", Query::CursorPosition),
    (b"\x1b[5n", Query::Status),
    (b"\x1b[c", Query::PrimaryAttributes),
    (b"\x1b[0c", Query::PrimaryAttributes),
    (b"\x1b[>c", Query::SecondaryAttributes),
    (b"\x1b[>0c", Query::SecondaryAttributes),
];

#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    Data(Vec<u8>),
    Query(Query),
}

#[derive(Default)]
pub struct Scanner {
    held: Vec<u8>,
}

enum Match {
    Full(usize, Query),
    Partial,
    None,
}

fn match_at(bytes: &[u8]) -> Match {
    let mut partial = false;
    for (p, q) in PATTERNS {
        if bytes.len() >= p.len() {
            if &bytes[..p.len()] == *p {
                return Match::Full(p.len(), *q);
            }
        } else if p.starts_with(bytes) {
            partial = true;
        }
    }
    if partial {
        Match::Partial
    } else {
        Match::None
    }
}

impl Scanner {
    /// Splits a chunk into output to pass on and queries to answer, in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Piece> {
        let mut bytes = std::mem::take(&mut self.held);
        bytes.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut data_from = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != 0x1b {
                i += 1;
                continue;
            }
            match match_at(&bytes[i..]) {
                Match::Full(len, q) => {
                    if i > data_from {
                        out.push(Piece::Data(bytes[data_from..i].to_vec()));
                    }
                    out.push(Piece::Query(q));
                    i += len;
                    data_from = i;
                }
                Match::Partial => {
                    if i > data_from {
                        out.push(Piece::Data(bytes[data_from..i].to_vec()));
                    }
                    self.held = bytes[i..].to_vec();
                    return out;
                }
                Match::None => i += 1,
            }
        }
        if data_from < bytes.len() {
            out.push(Piece::Data(bytes[data_from..].to_vec()));
        }
        out
    }

    /// Whether a partial query is held back.
    pub fn holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// Gives up on what is held: it never became a query, so it is output after all.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

/// The host's answer to a query, from the screen model.
pub fn answer(q: Query, screen: &vt100::Screen) -> Vec<u8> {
    match q {
        Query::CursorPosition => {
            let (row, col) = screen.cursor_position();
            format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
        }
        Query::Status => b"\x1b[0n".to_vec(),
        Query::PrimaryAttributes => b"\x1b[?62;22c".to_vec(),
        Query::SecondaryAttributes => b"\x1b[>0;10;1c".to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<Piece> {
        let mut s = Scanner::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.feed(c));
        }
        let rest = s.flush();
        if !rest.is_empty() {
            out.push(Piece::Data(rest));
        }
        // Adjacent data pieces are one stream to a viewer.
        let mut merged: Vec<Piece> = Vec::new();
        for p in out {
            match (merged.last_mut(), p) {
                (Some(Piece::Data(a)), Piece::Data(b)) => a.extend(b),
                (_, p) => merged.push(p),
            }
        }
        merged
    }

    #[test]
    fn takes_queries_out_wherever_the_chunks_split() {
        let stream: &[u8] = b"hi\x1b[6nthere\x1b[31m\x1b[>0c\x1b[c!";
        let expected = vec![
            Piece::Data(b"hi".to_vec()),
            Piece::Query(Query::CursorPosition),
            Piece::Data(b"there\x1b[31m".to_vec()),
            Piece::Query(Query::SecondaryAttributes),
            Piece::Query(Query::PrimaryAttributes),
            Piece::Data(b"!".to_vec()),
        ];
        for a in 0..stream.len() {
            for b in a..stream.len() {
                assert_eq!(
                    run(&[&stream[..a], &stream[a..b], &stream[b..]]),
                    expected,
                    "split at {a}, {b}"
                );
            }
        }
    }

    #[test]
    fn leaves_other_sequences_alone() {
        assert_eq!(
            run(&[b"\x1b[6;1H\x1b[5m\x1b[?1049h\x1b"]),
            vec![Piece::Data(b"\x1b[6;1H\x1b[5m\x1b[?1049h\x1b".to_vec())]
        );
    }

    #[test]
    fn answers_from_the_model() {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(b"\x1b[5;9H");
        assert_eq!(answer(Query::CursorPosition, p.screen()), b"\x1b[5;9R");
        assert_eq!(answer(Query::Status, p.screen()), b"\x1b[0n");
    }
}
