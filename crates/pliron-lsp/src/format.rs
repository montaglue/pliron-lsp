//! pliron's declarative op format language (the `format = "..."` strings of
//! `#[pliron_op]` / `#[format_op]`), parsed for completion snippets and
//! signature help.
//!
//! ```text
//! "$0 ` <` attr($llvm_icmp_predicate, $ICmpPredicateAttr) `> ` $1 ` : ` type($0)"
//! ```

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Elem {
    /// Literal text (between backticks), including its spaces.
    Lit(String),
    /// `$i`: operand i.
    Operand(u32),
    /// `operands(sep)`.
    Operands,
    /// `$name`: an attribute of the op.
    Attr(String),
    /// `attr(...)` / `opt_attr(...)` with a statically known attribute type.
    TypedAttr {
        name: String,
        label: Option<String>,
        delimiters: Option<(String, String)>,
        optional: bool,
    },
    /// `type($i)`: type of result i.
    Type(u32),
    /// `types(sep)`.
    Types,
    /// `typesig`.
    TypeSig,
    /// `opdtype($i)` / `opdtypes(sep)`.
    OpdType,
    /// `succ($i)` / `successors(sep)`.
    Succ,
    /// `region($i)` / `regions(sep)`.
    Region,
    /// `attr_dict`.
    AttrDict,
}

impl Elem {
    /// Does this element stand for user-provided content?
    pub fn is_placeholder(&self) -> bool {
        !matches!(self, Elem::Lit(_) | Elem::AttrDict)
            && !matches!(self, Elem::TypedAttr { optional: true, .. })
    }
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut in_lit = false;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '`' => in_lit = !in_lit,
            '(' if !in_lit => depth += 1,
            ')' if !in_lit => depth -= 1,
            ',' if !in_lit && depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn var(s: &str) -> Option<&str> {
    s.trim().strip_prefix('$')
}

fn index_of(s: &str) -> u32 {
    var(s).and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn literal(s: &str) -> Option<String> {
    let s = s.trim();
    s.strip_prefix('`')
        .and_then(|s| s.strip_suffix('`'))
        .map(str::to_string)
}

/// Parse a format string. Unknown directives are skipped.
pub fn parse(fmt: &str) -> Vec<Elem> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '`' {
            let start = i + 1;
            i = start;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            out.push(Elem::Lit(chars[start..i.min(chars.len())].iter().collect()));
            i += 1;
        } else if c == '$' {
            let start = i + 1;
            i = start;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            out.push(match name.parse::<u32>() {
                Ok(n) => Elem::Operand(n),
                Err(_) => Elem::Attr(name),
            });
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            let mut args = String::new();
            if i < chars.len() && chars[i] == '(' {
                let mut depth = 0;
                let mut in_lit = false;
                let a = i + 1;
                while i < chars.len() {
                    match chars[i] {
                        '`' => in_lit = !in_lit,
                        '(' if !in_lit => depth += 1,
                        ')' if !in_lit => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                args = chars[a..i.min(chars.len())].iter().collect();
                i += 1;
            }
            let a = split_args(&args);
            let elem = match name.as_str() {
                "type" => Some(Elem::Type(a.first().map(|s| index_of(s)).unwrap_or(0))),
                "types" => Some(Elem::Types),
                "typesig" => Some(Elem::TypeSig),
                "opdtype" | "opdtypes" => Some(Elem::OpdType),
                "operands" => Some(Elem::Operands),
                "succ" | "successors" => Some(Elem::Succ),
                "region" | "regions" => Some(Elem::Region),
                "attr_dict" => Some(Elem::AttrDict),
                "attr" | "opt_attr" => {
                    let attr = a.first().and_then(|s| var(s)).unwrap_or("attr").to_string();
                    let mut label = None;
                    let mut delimiters = None;
                    for extra in a.iter().skip(2) {
                        if let Some(inner) =
                            extra.strip_prefix("label(").and_then(|s| s.strip_suffix(')'))
                        {
                            label = var(inner).map(str::to_string).or_else(|| literal(inner));
                        } else if let Some(inner) =
                            extra.strip_prefix("delimiters(").and_then(|s| s.strip_suffix(')'))
                        {
                            let d = split_args(inner);
                            if let (Some(o), Some(c)) = (
                                d.first().and_then(|s| literal(s)),
                                d.get(1).and_then(|s| literal(s)),
                            ) {
                                delimiters = Some((o, c));
                            }
                        }
                    }
                    Some(Elem::TypedAttr {
                        name: attr,
                        label,
                        delimiters,
                        optional: name == "opt_attr",
                    })
                }
                _ => None,
            };
            out.extend(elem);
        } else {
            i += 1;
        }
    }
    out
}

/// Strip common dialect prefixes from attribute names for placeholders
/// (`llvm_icmp_predicate` -> `predicate`).
fn short_attr(name: &str) -> &str {
    name.rsplit('_').next().filter(|s| !s.is_empty()).unwrap_or(name)
}

/// One rendered piece of a format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    /// Is this a placeholder (a parameter) rather than literal syntax?
    pub placeholder: bool,
}

/// Render a parsed format as pieces, naming operands after `operand_names`.
pub fn render(elems: &[Elem], operand_names: &[String]) -> Vec<Piece> {
    let opd = |n: u32| {
        operand_names
            .get(n as usize)
            .filter(|s| *s != "_")
            .cloned()
            .unwrap_or_else(|| format!("opd{n}"))
    };
    let mut out = Vec::new();
    let mut push = |text: String, placeholder: bool| out.push(Piece { text, placeholder });
    for e in elems {
        match e {
            Elem::Lit(l) => push(l.clone(), false),
            Elem::Operand(n) => push(opd(*n), true),
            Elem::Operands => push("operands".into(), true),
            Elem::Attr(a) => push(short_attr(a).to_string(), true),
            Elem::TypedAttr {
                name,
                label,
                delimiters,
                optional,
            } => {
                if *optional {
                    continue;
                }
                if let Some((o, _)) = delimiters {
                    push(o.clone(), false);
                }
                if let Some(l) = label {
                    push(format!("{l} "), false);
                }
                push(short_attr(name).to_string(), true);
                if let Some((_, c)) = delimiters {
                    push(c.clone(), false);
                }
            }
            Elem::Type(_) | Elem::OpdType => push("type".into(), true),
            Elem::Types => push("types".into(), true),
            Elem::TypeSig => push("(types) -> (types)".into(), true),
            Elem::Succ => push("^bb".into(), true),
            Elem::Region => push("{ region }".into(), true),
            Elem::AttrDict => {}
        }
    }
    out
}

/// Plain text of the rendered pieces (for signature labels).
pub fn display(pieces: &[Piece]) -> String {
    let mut s = String::new();
    for p in pieces {
        s.push_str(&p.text);
    }
    s
}

fn escape_snippet(s: &str) -> String {
    s.replace('\\', "\\\\").replace('$', "\\$").replace('}', "\\}")
}

/// An LSP snippet for the rendered pieces (`${1:lhs}, ${2:rhs} : ${3:type}`).
pub fn snippet(pieces: &[Piece]) -> String {
    let mut s = String::new();
    let mut n = 1;
    for p in pieces {
        if p.placeholder {
            s.push_str(&format!("${{{n}:{}}}", escape_snippet(&p.text)));
            n += 1;
        } else {
            s.push_str(&escape_snippet(&p.text));
        }
    }
    s
}

/// Which placeholder (0-based) the cursor is in, given the text typed after
/// the op name: literals must appear in order; the active placeholder is the
/// last one whose preceding literals were all typed.
pub fn active_placeholder(pieces: &[Piece], typed: &str) -> Option<u32> {
    let mut rest = typed;
    let mut current = None;
    let mut idx = 0;
    for p in pieces {
        if p.placeholder {
            current = Some(idx);
            idx += 1;
        } else {
            let lit = p.text.trim();
            if lit.is_empty() {
                continue;
            }
            match rest.find(lit) {
                Some(i) => rest = &rest[i + lit.len()..],
                None => break,
            }
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icmp_format() {
        let f = "$0 ` <` attr($llvm_icmp_predicate, $ICmpPredicateAttr) `> ` $1 ` : ` type($0)";
        let e = parse(f);
        assert_eq!(
            e[0..2],
            [Elem::Operand(0), Elem::Lit(" <".into())]
        );
        let p = render(&e, &["lhs".into(), "rhs".into()]);
        assert_eq!(display(&p), "lhs <predicate> rhs : type");
        assert_eq!(snippet(&p), "${1:lhs} <${2:predicate}> ${3:rhs} : ${4:type}");
    }

    #[test]
    fn alloca_with_optional_attr() {
        let f = "`[` attr($llvm_alloca_element_type, $TypeAttr) ` x ` $0 `]` ` ` opt_attr($llvm_alignment, $AlignmentAttr, label($align), delimiters(`[`, `]`)) ` : ` type($0)";
        let p = render(&parse(f), &["array_size".into()]);
        assert_eq!(display(&p), "[type x array_size]  : type");
    }

    #[test]
    fn active_parameter() {
        let p = render(&parse("$0 `, ` $1 ` : ` type($0)"), &[]);
        assert_eq!(active_placeholder(&p, " x"), Some(0));
        assert_eq!(active_placeholder(&p, " x, y"), Some(1));
        assert_eq!(active_placeholder(&p, " x, y : "), Some(2));
    }

    #[test]
    fn snippet_escapes() {
        let p = render(&parse("`{` $0 `}`"), &[]);
        assert_eq!(snippet(&p), "{${1:opd0}\\}");
    }
}
