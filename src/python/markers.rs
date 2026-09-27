//! PEP 508 markers evaluated against the interpreter being provisioned, never
//! the Python (or Rust platform) of the process invoking kong.
use std::collections::HashMap;
use std::path::Path;
use anyhow::{bail, Context, Result};
use super::pep440::{SpecifierSet, Version};

#[derive(Debug, Clone)]
pub struct MarkerEnvironment(pub HashMap<String, String>);

impl MarkerEnvironment {
    pub fn from_interpreter(executable: &Path) -> Result<Self> {
        let output = std::process::Command::new(executable).args(["-c", r#"
import json, os, platform, sys
print(json.dumps(dict(sys_platform=sys.platform, platform_system=platform.system(),
os_name=os.name, platform_machine=platform.machine(), python_version='.'.join(map(str, sys.version_info[:2])),
python_full_version=platform.python_version(), implementation_name=sys.implementation.name,
implementation_version=platform.python_version(), platform_python_implementation=platform.python_implementation(),
platform_release=platform.release(), platform_version=platform.version())))
"#]).output().context("failed to query target Python marker environment")?;
        if !output.status.success() {
            bail!("target Python marker query failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(Self(serde_json::from_slice(&output.stdout).context("invalid target Python marker environment")?))
    }

    pub fn evaluate(&self, marker: &str, extras: &[String]) -> Result<bool> {
        if marker.trim().is_empty() { return Ok(true); }
        let tokens = tokenize(marker)?;
        // Base dependencies are always installed; requested extras add to them.
        let mut matched = false;
        for extra in std::iter::once("").chain(extras.iter().map(String::as_str)) {
            let mut parser = Parser { tokens: &tokens, pos: 0, env: self, extra };
            matched |= parser.or()?;
            if parser.pos != tokens.len() { bail!("unexpected token in marker: {marker}"); }
        }
        Ok(matched)
    }
}

#[derive(Debug, PartialEq)]
enum Token { Word(String), Literal(String), Op(String), Left, Right }
fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {},
            '(' => tokens.push(Token::Left), ')' => tokens.push(Token::Right),
            '\'' | '"' => {
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some(end) if end == c => break,
                        Some(x) => value.push(x),
                        None => bail!("unterminated marker string"),
                    }
                }
                tokens.push(Token::Literal(value));
            }
            '=' | '!' | '<' | '>' | '~' => {
                let mut op = c.to_string();
                while chars.peek() == Some(&'=') { op.push(chars.next().unwrap()); }
                if !["==", "!=", "<=", ">=", "<", ">", "~=", "==="].contains(&op.as_str()) {
                    bail!("invalid marker operator: {op}");
                }
                tokens.push(Token::Op(op));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut word = c.to_string();
                while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                    word.push(chars.next().unwrap());
                }
                tokens.push(Token::Word(word));
            }
            _ => bail!("invalid character in marker: {c}"),
        }
    }
    Ok(tokens)
}
struct Parser<'a> { tokens: &'a [Token], pos: usize, env: &'a MarkerEnvironment, extra: &'a str }
impl Parser<'_> {
    fn word(&mut self, word: &str) -> bool {
        if matches!(self.tokens.get(self.pos), Some(Token::Word(w)) if w == word) {
            self.pos += 1; true
        } else { false }
    }
    fn or(&mut self) -> Result<bool> {
        let mut value = self.and()?;
        while self.word("or") { value |= self.and()?; }
        Ok(value)
    }
    fn and(&mut self) -> Result<bool> {
        let mut value = self.atom()?;
        while self.word("and") { value &= self.atom()?; }
        Ok(value)
    }
    fn operand(&mut self) -> Result<(String, bool)> {
        let token = self.tokens.get(self.pos).context("missing marker operand")?;
        self.pos += 1;
        match token {
            Token::Literal(s) => Ok((s.clone(), false)),
            Token::Word(name) if name == "extra" => Ok((self.extra.to_string(), true)),
            Token::Word(name) => Ok((self.env.0.get(name).with_context(|| format!("unknown marker variable: {name}"))?.clone(), false)),
            _ => bail!("expected marker variable or quoted string"),
        }
    }
    fn atom(&mut self) -> Result<bool> {
        if self.tokens.get(self.pos) == Some(&Token::Left) {
            self.pos += 1;
            let value = self.or()?;
            if self.tokens.get(self.pos) != Some(&Token::Right) { bail!("unclosed marker parenthesis"); }
            self.pos += 1;
            return Ok(value);
        }
        let (mut left, left_extra) = self.operand()?;
        let op = if let Some(Token::Op(op)) = self.tokens.get(self.pos) {
            self.pos += 1; op.clone()
        } else if self.word("in") { "in".into() }
        else if self.word("not") && self.word("in") { "not in".into() }
        else { bail!("missing marker comparison operator"); };
        let (mut right, right_extra) = self.operand()?;
        if left_extra || right_extra {
            left = normalize_extra(&left); right = normalize_extra(&right);
        }
        if op == "in" { return Ok(right.contains(&left)); }
        if op == "not in" { return Ok(!right.contains(&left)); }
        if op == "===" { return Ok(left == right); }
        // Version comparisons use PEP 440 (3.10 > 3.9, ~=, ==3.10.*).
        let spec = SpecifierSet::parse(&format!("{op}{right}"));
        if let Some(version) = Version::parse(&left) {
            if !spec.is_empty() { return Ok(spec.matches(&version)); }
        }
        Ok(match op.as_str() {
            "==" => left == right, "!=" => left != right,
            "<" => left < right, "<=" => left <= right,
            ">" => left > right, ">=" => left >= right,
            _ => bail!("invalid string marker comparison: {op}"),
        })
    }
}

pub fn normalize_extra(value: &str) -> String {
    value.to_ascii_lowercase().split(['-', '_', '.']).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-")
}

#[cfg(test)]
pub fn linux_fixture() -> MarkerEnvironment {
    MarkerEnvironment([
        ("sys_platform", "linux"), ("platform_system", "Linux"), ("os_name", "posix"),
        ("platform_machine", "x86_64"), ("python_version", "3.10"), ("python_full_version", "3.10.21"),
        ("implementation_name", "cpython"), ("platform_python_implementation", "CPython"),
    ].into_iter().map(|(k,v)| (k.into(),v.into())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_markers_boolean_versions_and_extras() {
        let env = linux_fixture();
        for marker in [
            "sys_platform == 'linux'", "platform_system == 'Linux' and os_name == 'posix'",
            "platform_machine in 'x86_64,aarch64'", "implementation_name == 'cpython'",
            "platform_python_implementation == 'CPython'", "python_version > '3.9'",
            "python_full_version >= '3.10.20'", "python_full_version ~= '3.10.0'",
            "python_version == '3.10.*'", "'3.9' < python_version",
            "sys_platform == 'win32' or (python_version >= '3.10' and os_name != 'nt')",
            "sys_platform == 'linux' or os_name == 'nt' and python_version < '3'",
            "sys_platform not in 'win32,darwin'", "extra == 'SECURITY.test'",
        ] { assert!(env.evaluate(marker, &["security-test".into()]).unwrap(), "{marker}"); }
        for marker in ["sys_platform == 'win32'", "python_version < '3.9'", "extra == 'cli'",
            "(sys_platform == 'linux' or os_name == 'nt') and python_version < '3'",
            "python_full_version > '3.10.21'", "python_version != '3.10'"] {
            assert!(!env.evaluate(marker, &[]).unwrap(), "{marker}");
        }
        assert!(env.evaluate("extra == 'cli'", &["cli".into()]).unwrap());
        assert!(!env.evaluate("extra == 'cli' and sys_platform == 'win32'", &["cli".into()]).unwrap());
        for invalid in ["unknown == 'x'", "python_version >=", "(os_name == 'posix'", "os_name = 'x'", "os_name == 'posix' or garbage", "extra == 'cli' trailing"] {
            assert!(env.evaluate(invalid, &[]).is_err(), "{invalid}");
        }
        let mut windows = env.clone();
        windows.0.insert("sys_platform".into(), "win32".into());
        assert!(windows.evaluate("sys_platform == 'win32'", &[]).unwrap());
    }
}
