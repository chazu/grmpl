//! A tiny tokenizer for the surface syntax. Skips whitespace and `//` comments.

#[derive(Clone, PartialEq, Debug)]
pub enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Arrow, // ->
    Eq,    // =
    Tilde, // ~
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
            '(' => { out.push(Token::LParen); i += 1; }
            ')' => { out.push(Token::RParen); i += 1; }
            '{' => { out.push(Token::LBrace); i += 1; }
            '}' => { out.push(Token::RBrace); i += 1; }
            ',' => { out.push(Token::Comma); i += 1; }
            '=' => { out.push(Token::Eq); i += 1; }
            '~' => { out.push(Token::Tilde); i += 1; }
            '-' if i + 1 < bytes.len() && bytes[i + 1] == '>' => {
                out.push(Token::Arrow);
                i += 2;
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
            c if c.is_ascii_digit() || (c == '-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit()) => {
                let start = i;
                if c == '-' {
                    i += 1;
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = bytes[start..i].iter().collect();
                let n: i64 = text.parse().map_err(|_| format!("bad integer `{text}`"))?;
                out.push(Token::Int(n));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_' || bytes[i] == '-') {
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
