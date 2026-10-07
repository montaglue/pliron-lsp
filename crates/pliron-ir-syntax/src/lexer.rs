//! A lexer for pliron textual IR that never fails.
//!
//! pliron has no fixed grammar after an op name (every op/type/attribute
//! brings its own parser), so the lexer only recognises token shapes that are
//! common to all of pliron's syntax. Anything else becomes
//! [`TokenKind::Other`], which is never an error by itself.

/// Byte offset into the document text.
pub type Offset = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// `name`: an SSA value, keyword or a word inside some syntax.
    Ident,
    /// `a.b` / `a.b.c`: an op, type or attribute name.
    QualName,
    /// `^label`
    BlockLabel,
    /// `@symbol`
    SymbolRef,
    /// `!3`: a reference to an outlined attribute entry.
    OutlineRef,
    Number,
    String,
    /// `->`
    Arrow,
    /// One of `{}()[]<>,;:=*?|+-!@#$%&/\~^.`
    Punct(char),
    /// Anything else.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: Offset,
    pub end: Offset,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start as usize..self.end as usize]
    }

    /// The name without its sigil (`^`, `@` or `!`).
    pub fn name<'a>(&self, src: &'a str) -> &'a str {
        match self.kind {
            TokenKind::BlockLabel | TokenKind::SymbolRef | TokenKind::OutlineRef => {
                &src[self.start as usize + 1..self.end as usize]
            }
            _ => self.text(src),
        }
    }

    pub fn is_punct(&self, c: char) -> bool {
        self.kind == TokenKind::Punct(c)
    }

    pub fn contains(&self, offset: Offset) -> bool {
        self.start <= offset && offset < self.end
    }

    /// Like [`Token::contains`], but also true when `offset` is right after
    /// the token (cursor at the end of a word).
    pub fn touches(&self, offset: Offset) -> bool {
        self.start <= offset && offset <= self.end
    }
}

/// A lexing problem (the token is still produced).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    pub start: Offset,
    pub end: Offset,
    pub message: String,
}

pub fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

pub fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Is `s` a valid pliron identifier?
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if is_ident_start(c)) && chars.all(is_ident_continue)
}

pub fn lex(src: &str) -> (Vec<Token>, Vec<LexError>) {
    Lexer {
        src,
        pos: 0,
        tokens: Vec::new(),
        errors: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    errors: Vec<LexError>,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, byte_off: usize) -> Option<char> {
        self.src.get(self.pos + byte_off..)?.chars().next()
    }

    fn bump_while(&mut self, f: impl Fn(char) -> bool) {
        while let Some(c) = self.peek() {
            if !f(c) {
                break;
            }
            self.pos += c.len_utf8();
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            start: start as Offset,
            end: self.pos as Offset,
        });
    }

    fn run(mut self) -> (Vec<Token>, Vec<LexError>) {
        while let Some(c) = self.peek() {
            let start = self.pos;
            if c.is_whitespace() {
                self.bump_while(char::is_whitespace);
                continue;
            }
            if c == '"' {
                self.string(start);
                continue;
            }
            if is_ident_start(c) {
                self.ident_or_qualname(start);
                continue;
            }
            if c.is_ascii_digit()
                || (c == '-' && self.peek_at(1).is_some_and(|d| d.is_ascii_digit()))
            {
                self.number(start);
                continue;
            }
            if (c == '^' || c == '@') && self.peek_at(1).is_some_and(is_ident_start) {
                self.pos += 1;
                self.bump_while(is_ident_continue);
                let kind = if c == '^' {
                    TokenKind::BlockLabel
                } else {
                    TokenKind::SymbolRef
                };
                self.push(kind, start);
                continue;
            }
            if c == '!' && self.peek_at(1).is_some_and(|d| d.is_ascii_digit()) {
                self.pos += 1;
                self.bump_while(|d| d.is_ascii_digit());
                self.push(TokenKind::OutlineRef, start);
                continue;
            }
            if c == '-' && self.peek_at(1) == Some('>') {
                self.pos += 2;
                self.push(TokenKind::Arrow, start);
                continue;
            }
            self.pos += c.len_utf8();
            let kind = if "{}()[]<>,;:=*?|+-!@#$%&/\\~^.".contains(c) {
                TokenKind::Punct(c)
            } else {
                TokenKind::Other
            };
            self.push(kind, start);
        }
        (self.tokens, self.errors)
    }

    fn string(&mut self, start: usize) {
        self.pos += 1;
        let mut terminated = false;
        while let Some(c) = self.peek() {
            self.pos += c.len_utf8();
            match c {
                '\\' => {
                    if let Some(n) = self.peek() {
                        self.pos += n.len_utf8();
                    }
                }
                '"' => {
                    terminated = true;
                    break;
                }
                _ => {}
            }
        }
        if !terminated {
            self.errors.push(LexError {
                start: start as Offset,
                end: self.pos as Offset,
                message: "unterminated string literal".into(),
            });
        }
        self.push(TokenKind::String, start);
    }

    fn ident_or_qualname(&mut self, start: usize) {
        let mut qualified = false;
        loop {
            self.bump_while(is_ident_continue);
            // `.` continues the name only when directly followed by an
            // identifier start (`builtin.integer`), not in `a. b` or `1.5`.
            if self.peek() == Some('.') && self.peek_at(1).is_some_and(is_ident_start) {
                qualified = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let kind = if qualified {
            TokenKind::QualName
        } else {
            TokenKind::Ident
        };
        self.push(kind, start);
    }

    fn number(&mut self, start: usize) {
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        loop {
            self.bump_while(|c| c.is_ascii_alphanumeric() || c == '_');
            if self.peek() == Some('.') && self.peek_at(1).is_some_and(|d| d.is_ascii_digit()) {
                self.pos += 1;
            } else if matches!(self.peek(), Some('+' | '-'))
                && self.src[..self.pos].ends_with(['e', 'E'])
                && self.peek_at(1).is_some_and(|d| d.is_ascii_digit())
            {
                // Exponent sign: 1.5e-3
                self.pos += 1;
            } else {
                break;
            }
        }
        self.push(TokenKind::Number, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(TokenKind, &str)> {
        let (toks, errs) = lex(src);
        assert!(errs.is_empty(), "{errs:?}");
        toks.iter().map(|t| (t.kind, t.text(src))).collect()
    }

    #[test]
    fn basic_statement() {
        use TokenKind::*;
        assert_eq!(
            kinds("sum = llvm.add a, b <{nsw=false}> : builtin.integer i64;"),
            vec![
                (Ident, "sum"),
                (Punct('='), "="),
                (QualName, "llvm.add"),
                (Ident, "a"),
                (Punct(','), ","),
                (Ident, "b"),
                (Punct('<'), "<"),
                (Punct('{'), "{"),
                (Ident, "nsw"),
                (Punct('='), "="),
                (Ident, "false"),
                (Punct('}'), "}"),
                (Punct('>'), ">"),
                (Punct(':'), ":"),
                (QualName, "builtin.integer"),
                (Ident, "i64"),
                (Punct(';'), ";"),
            ]
        );
    }

    #[test]
    fn sigils_numbers_arrows() {
        use TokenKind::*;
        assert_eq!(
            kinds("^bb0(x: t) @foo !12 -> -3 1.5e-3 0x1F !outlined"),
            vec![
                (BlockLabel, "^bb0"),
                (Punct('('), "("),
                (Ident, "x"),
                (Punct(':'), ":"),
                (Ident, "t"),
                (Punct(')'), ")"),
                (SymbolRef, "@foo"),
                (OutlineRef, "!12"),
                (Arrow, "->"),
                (Number, "-3"),
                (Number, "1.5e-3"),
                (Number, "0x1F"),
                (Punct('!'), "!"),
                (Ident, "outlined"),
            ]
        );
    }

    #[test]
    fn strings_and_unicode() {
        use TokenKind::*;
        assert_eq!(
            kinds(r#"x = test.s "a \"q\" é" ; αβ"#),
            vec![
                (Ident, "x"),
                (Punct('='), "="),
                (QualName, "test.s"),
                (String, r#""a \"q\" é""#),
                (Punct(';'), ";"),
                (Ident, "αβ"),
            ]
        );
        let (toks, errs) = lex("\"open");
        assert_eq!(toks.len(), 1);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn identifier_check() {
        assert!(is_identifier("x_1"));
        assert!(is_identifier("_"));
        assert!(!is_identifier("1x"));
        assert!(!is_identifier("a.b"));
        assert!(!is_identifier(""));
    }
}
