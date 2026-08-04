//! A tiny tokenizer for the surface syntax. Skips whitespace and `//` comments.

use grmpl_core::FiniteF64;

#[derive(Clone, PartialEq, Debug)]
pub enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    Float(FiniteF64),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket, // [
    RBracket, // ]
    Comma,
    Colon,   // :
    Arrow,   // ->
    Eq,      // =
    EqEq,    // ==
    Ne,      // !=
    Lt,      // <
    Le,      // <=
    Gt,      // >
    Ge,      // >=
    Plus,    // +
    Minus,   // -
    Star,    // *
    Slash,   // /
    Percent, // %
    Bang,    // !
    AndAnd,  // &&
    OrOr,    // ||
    Tilde,   // ~
}

pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '/' if i + 1 < bytes.len() && bytes[i + 1] == '/' => {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
            }
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            '{' => {
                out.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                out.push(Token::RBrace);
                i += 1;
            }
            '[' => {
                out.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Token::RBracket);
                i += 1;
            }
            ',' => {
                out.push(Token::Comma);
                i += 1;
            }
            ':' => {
                out.push(Token::Colon);
                i += 1;
            }
            '=' if i + 1 < bytes.len() && bytes[i + 1] == '=' => {
                out.push(Token::EqEq);
                i += 2;
            }
            '=' => {
                out.push(Token::Eq);
                i += 1;
            }
            '!' if i + 1 < bytes.len() && bytes[i + 1] == '=' => {
                out.push(Token::Ne);
                i += 2;
            }
            '!' => {
                out.push(Token::Bang);
                i += 1;
            }
            '<' if i + 1 < bytes.len() && bytes[i + 1] == '=' => {
                out.push(Token::Le);
                i += 2;
            }
            '<' => {
                out.push(Token::Lt);
                i += 1;
            }
            '>' if i + 1 < bytes.len() && bytes[i + 1] == '=' => {
                out.push(Token::Ge);
                i += 2;
            }
            '>' => {
                out.push(Token::Gt);
                i += 1;
            }
            '&' if i + 1 < bytes.len() && bytes[i + 1] == '&' => {
                out.push(Token::AndAnd);
                i += 2;
            }
            '|' if i + 1 < bytes.len() && bytes[i + 1] == '|' => {
                out.push(Token::OrOr);
                i += 2;
            }
            '+' => {
                out.push(Token::Plus);
                i += 1;
            }
            '~' => {
                out.push(Token::Tilde);
                i += 1;
            }
            '-' if i + 1 < bytes.len() && bytes[i + 1] == '>' => {
                out.push(Token::Arrow);
                i += 2;
            }
            '-' => {
                out.push(Token::Minus);
                i += 1;
            }
            '*' => {
                out.push(Token::Star);
                i += 1;
            }
            '%' => {
                out.push(Token::Percent);
                i += 1;
            }
            '/' => {
                out.push(Token::Slash);
                i += 1;
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                while i < bytes.len() && bytes[i] != '"' {
                    s.push(bytes[i]);
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err("unterminated string literal".into());
                }
                i += 1; // closing quote
                out.push(Token::Str(s));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let mut is_float = false;
                if i < bytes.len() && bytes[i] == '.' {
                    is_float = true;
                    i += 1;
                    let fractional = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == fractional {
                        return Err("a decimal point must be followed by digits".into());
                    }
                }
                if i < bytes.len() && matches!(bytes[i], 'e' | 'E') {
                    is_float = true;
                    i += 1;
                    if i < bytes.len() && matches!(bytes[i], '+' | '-') {
                        i += 1;
                    }
                    let exponent = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == exponent {
                        return Err("a float exponent must contain digits".into());
                    }
                }
                let text: String = bytes[start..i].iter().collect();
                if is_float {
                    let n: f64 = text.parse().map_err(|_| format!("bad float `{text}`"))?;
                    let n = FiniteF64::new(n)
                        .ok_or_else(|| format!("float literal `{text}` is not finite"))?;
                    out.push(Token::Float(n));
                } else {
                    let n: i64 = text.parse().map_err(|_| format!("bad integer `{text}`"))?;
                    out.push(Token::Int(n));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                    i += 1;
                }
                let text: String = bytes[start..i].iter().collect();
                out.push(Token::Ident(text));
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_integer_and_float_literals() {
        assert_eq!(
            lex("1 1.0 0.25 1e3 2.5e-2").unwrap(),
            vec![
                Token::Int(1),
                Token::Float(FiniteF64::new(1.0).unwrap()),
                Token::Float(FiniteF64::new(0.25).unwrap()),
                Token::Float(FiniteF64::new(1000.0).unwrap()),
                Token::Float(FiniteF64::new(0.025).unwrap()),
            ]
        );
    }

    #[test]
    fn rejects_non_finite_and_malformed_float_literals() {
        assert!(lex("1e9999").unwrap_err().contains("not finite"));
        assert!(lex("1.").is_err());
        assert!(lex("1e+").is_err());
    }

    #[test]
    fn tokenizes_expression_operators_without_stealing_arrow_or_comments() {
        assert_eq!(
            lex("a-1 <= b && b != 0 -> x // ignored\n c/2").unwrap(),
            vec![
                Token::Ident("a".into()),
                Token::Minus,
                Token::Int(1),
                Token::Le,
                Token::Ident("b".into()),
                Token::AndAnd,
                Token::Ident("b".into()),
                Token::Ne,
                Token::Int(0),
                Token::Arrow,
                Token::Ident("x".into()),
                Token::Ident("c".into()),
                Token::Slash,
                Token::Int(2),
            ]
        );
    }
}
