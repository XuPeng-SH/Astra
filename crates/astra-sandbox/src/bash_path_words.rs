use std::{borrow::Cow, iter::Peekable, str::Chars};

pub enum BashPathToken {
    Word(String),
    Redirect(String),
}

/// A workspace check examines shell path expressions. An operand check
/// separates redirections and passes the command's literal punctuation on
/// to the invoked program's operand grammar.
#[derive(Clone, Copy)]
pub enum BashPathScan {
    PathExpressions,
    CommandOperands,
}

/// Split shell words while keeping literal syntax distinct from expansion
/// syntax. This is a lexical view for path checks. Bash owns execution and
/// dynamic expansion.
pub fn bash_path_tokens(input: &str, scan: BashPathScan) -> Vec<BashPathToken> {
    use BashPathToken::{Redirect, Word};

    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut plain_number = true;
    let mut chars = input.chars().peekable();
    let mut quote = None;
    let path_expressions = matches!(scan, BashPathScan::PathExpressions);

    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' if quote.is_none() || quote == Some(ch) => {
                quote = if quote.is_some() { None } else { Some(ch) };
                started = true;
                plain_number = false;
            }
            '\\' if quote != Some('\'') => {
                if quote == Some('"')
                    && chars
                        .peek()
                        .is_some_and(|next| !matches!(next, '$' | '`' | '"' | '\\' | '\n'))
                {
                    word.push(ch);
                } else if let Some(next) = chars.next() {
                    if matches!(next, '\n' | '\r') {
                        continue;
                    }
                    push_literal(&mut word, next, path_expressions);
                }
                started = true;
                plain_number = false;
            }
            '$' if quote != Some('\'') => {
                push_parameter_or_dollar(&mut word, &mut chars, quote, path_expressions);
                started = true;
                plain_number = false;
            }
            '(' | ')' | ';' if !path_expressions && quote.is_none() => {
                word.push(LITERAL);
                word.push(ch);
                started = true;
                plain_number = false;
            }
            '>' | '<' if !path_expressions && quote.is_none() => {
                let mut op = String::new();
                if started && plain_number && !word.is_empty() {
                    op.push_str(&word);
                    word.clear();
                } else if started {
                    tokens.push(Word(std::mem::take(&mut word)));
                }
                started = false;
                plain_number = true;
                op.push(ch);
                if chars.next_if_eq(&ch).is_some() {
                    op.push(ch);
                }
                let operator = op.trim_start_matches(|ch: char| ch.is_ascii_digit());
                let suffix = match operator {
                    "<<" => Some('<'),
                    "<" => Some('>'),
                    ">" => Some('|'),
                    _ => None,
                };
                if let Some(suffix) = suffix
                    && chars.next_if_eq(&suffix).is_some()
                {
                    op.push(suffix);
                }
                if chars.next_if_eq(&'&').is_some() {
                    op.push('&');
                }
                tokens.push(Redirect(op));
            }
            ch if ch.is_whitespace() && quote.is_none() => {
                if started {
                    tokens.push(Word(std::mem::take(&mut word)));
                }
                started = false;
                plain_number = true;
            }
            _ => {
                if quote.is_some() || ch == '~' && started {
                    push_literal(&mut word, ch, path_expressions);
                } else {
                    word.push(ch);
                }
                started = true;
                plain_number &= ch.is_ascii_digit() && quote.is_none();
            }
        }
    }

    if started {
        tokens.push(Word(word));
    }
    tokens
}

/// Consume a complete unbraced parameter before merging adjacent fragments.
/// A terminator keeps e.g. `$HO"ME"` distinct from `$HOME`.
fn push_parameter_or_dollar(
    word: &mut String,
    chars: &mut Peekable<Chars<'_>>,
    quote: Option<char>,
    path_expressions: bool,
) {
    skip_line_continuations(chars);
    match chars.peek().copied() {
        Some(next) if next.is_ascii_alphabetic() || next == '_' => {
            word.push('$');
            while let Some(ch) = chars.next_if(|ch| ch.is_ascii_alphanumeric() || *ch == '_') {
                word.push(ch);
                skip_line_continuations(chars);
            }
            if matches!(chars.peek(), Some('"' | '\\'))
                || quote.is_none() && chars.peek() == Some(&'\'')
            {
                word.push_str(PARAMETER_END);
            }
        }
        Some(next)
            if next.is_ascii_digit() || matches!(next, '$' | '?' | '#' | '-' | '!' | '*' | '@') =>
        {
            word.push('$');
            word.push(next);
            chars.next();
        }
        Some('{') if path_expressions && quote == Some('"') => {
            word.push_str("${");
            chars.next();
            // Preserve simple braced references. Other expansions stay
            // unresolved rather than acquiring a guessed path.
            if let Some(len) = simple_parameter_len(chars.clone()) {
                word.extend(chars.by_ref().take(len));
            }
        }
        Some('{' | '(' | '[') => word.push('$'),
        Some('\'' | '"') if quote.is_none() => word.push('$'),
        _ => push_literal(word, '$', false),
    }
}

fn skip_line_continuations(chars: &mut Peekable<Chars<'_>>) {
    while chars.peek() == Some(&'\\') {
        let mut rest = chars.clone();
        rest.next();
        if rest.next() != Some('\n') {
            break;
        }
        *chars = rest;
    }
}

fn simple_parameter_len(mut chars: Peekable<Chars<'_>>) -> Option<usize> {
    let first = chars.next()?;
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }
    let mut len = 2;
    for ch in chars {
        match ch {
            '}' => return Some(len),
            ch if ch == '_' || ch.is_ascii_alphanumeric() => len += 1,
            _ => return None,
        }
    }
    None
}

// NUL cannot occur in an executable shell word. The two-byte annotations
// leave ordinary words unchanged and do not reserve any valid filename bytes.
const LITERAL: char = '\0';
const PARAMETER_END: &str = "\0v";

/// Ignore shell control punctuation the lexer found at an operand's edges.
/// Quoted or escaped punctuation is part of the filename.
pub fn trim_bash_control_punctuation(mut word: &str) -> &str {
    const CONTROLS: [&str; 3] = ["\0(", "\0)", "\0;"];
    while let Some(rest) = CONTROLS
        .iter()
        .find_map(|control| word.strip_prefix(control))
    {
        word = rest;
    }
    while let Some(rest) = CONTROLS
        .iter()
        .find_map(|control| word.strip_suffix(control))
    {
        word = rest;
    }
    word
}

fn literal_tag(ch: char, path_expressions: bool) -> Option<char> {
    Some(match ch {
        '~' => 't',
        '$' => 'd',
        '{' if path_expressions => 'o',
        '}' if path_expressions => 'c',
        ',' if path_expressions => 'm',
        _ => return None,
    })
}

fn push_literal(word: &mut String, ch: char, path_expressions: bool) {
    if let Some(tag) = literal_tag(ch, path_expressions) {
        word.push(LITERAL);
        word.push(tag);
    } else {
        word.push(ch);
    }
}

/// Mark literal argv from a parser that has already removed shell quoting.
pub fn mark_literal_bash_path_word(word: &mut String) {
    if word.contains(['~', '$']) {
        let mut marked = String::with_capacity(word.len());
        for ch in word.chars() {
            push_literal(&mut marked, ch, false);
        }
        *word = marked;
    }
}

/// The suffix of a recognized home-directory reference, before literal
/// annotations are decoded. An empty suffix denotes the home itself.
pub fn bash_home_suffix(word: &str) -> Option<&str> {
    word.strip_prefix('~')
        .and_then(directory_suffix)
        .or_else(|| bash_parameter_suffix(word, "HOME"))
}

pub fn bash_parameter_suffix<'a>(word: &'a str, parameter: &str) -> Option<&'a str> {
    let reference = word.strip_prefix('$')?;
    let suffix = if let Some(braced) = reference.strip_prefix('{') {
        braced.strip_prefix(parameter)?.strip_prefix('}')?
    } else {
        let suffix = reference.strip_prefix(parameter)?;
        suffix.strip_prefix(PARAMETER_END).unwrap_or(suffix)
    };
    directory_suffix(suffix)
}

fn directory_suffix(suffix: &str) -> Option<&str> {
    match suffix {
        "" => Some(""),
        tail => tail.strip_prefix('/'),
    }
}

/// Decode the lexical annotations before using a word as a filename or
/// passing the decoded text to a nested shell parser.
pub fn decode_bash_path_word(word: &str) -> Cow<'_, str> {
    if !word.contains(LITERAL) {
        return Cow::Borrowed(word);
    }

    let mut decoded = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == LITERAL && chars.next_if_eq(&'v').is_some() {
            continue;
        }
        if ch == LITERAL
            && let Some(literal) = match chars.peek() {
                Some('t') => Some('~'),
                Some('d') => Some('$'),
                Some('o') => Some('{'),
                Some('c') => Some('}'),
                Some('m') => Some(','),
                Some(control @ ('(' | ')' | ';')) => Some(*control),
                _ => None,
            }
        {
            chars.next();
            decoded.push(literal);
        } else {
            decoded.push(ch);
        }
    }
    Cow::Owned(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_reference_depends_on_source_parameter_boundaries() {
        for (source, expected) in [
            (r#""$HOME"/fixture"#, Some("fixture")),
            (r#""${HOME}/fixture""#, Some("fixture")),
            (r#""$"'HOME/fixture'"#, None),
            (r#"$HO"ME/fixture""#, None),
            (r#"$HO\ME/fixture"#, None),
            ("$HO\\\nME/fixture", Some("fixture")),
            (r#""$$HOME/fixture""#, None),
        ] {
            for scan in [BashPathScan::PathExpressions, BashPathScan::CommandOperands] {
                let tokens = bash_path_tokens(source, scan);
                let [BashPathToken::Word(word)] = tokens.as_slice() else {
                    panic!("expected one word for {source:?}");
                };
                assert_eq!(bash_home_suffix(word), expected, "{source:?}");
            }
        }
    }
}
