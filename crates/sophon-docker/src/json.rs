//! A minimal JSON parser — just enough for the Docker Engine API. No deps.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> &Value {
        match self {
            Value::Obj(m) => m.get(key).unwrap_or(&Value::Null),
            _ => &Value::Null,
        }
    }

    pub fn str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            _ => "",
        }
    }

    pub fn num(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            _ => 0.0,
        }
    }

    pub fn bool(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    pub fn arr(&self) -> &[Value] {
        match self {
            Value::Arr(a) => a,
            _ => &[],
        }
    }
}

/// Parses one JSON document. Returns `None` on malformed input.
pub fn parse(input: &[u8]) -> Option<Value> {
    let mut p = Parser { b: input, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    // Trailing garbage is tolerated (some servers append whitespace/newlines).
    Some(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

/// Recursion cap: Docker responses are shallow; this guards against hostile depth.
const MAX_DEPTH: usize = 64;

impl Parser<'_> {
    fn peek(&self) -> u8 {
        self.b.get(self.i).copied().unwrap_or(0)
    }

    fn ws(&mut self) {
        while matches!(self.peek(), b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.ws();
        match self.peek() {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Value::Str),
            b't' => self.lit(b"true", Value::Bool(true)),
            b'f' => self.lit(b"false", Value::Bool(false)),
            b'n' => self.lit(b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn lit(&mut self, word: &[u8], v: Value) -> Option<Value> {
        if self.b.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Some(v)
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        if self.peek() == b'-' {
            self.i += 1;
        }
        while matches!(self.peek(), b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i]).ok()?.parse::<f64>().ok().map(Value::Num)
    }

    fn string(&mut self) -> Option<String> {
        if self.peek() != b'"' {
            return None;
        }
        self.i += 1;
        // Collect raw bytes (multibyte UTF-8 stays intact), decode at the end.
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.b.get(self.i).copied()? {
                b'"' => {
                    self.i += 1;
                    return Some(String::from_utf8_lossy(&out).into_owned());
                }
                b'\\' => {
                    self.i += 1;
                    match self.b.get(self.i).copied()? {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = self.b.get(self.i + 1..self.i + 5)?;
                            let cp = u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(char::from_u32(cp).unwrap_or('\u{fffd}').encode_utf8(&mut buf).as_bytes());
                            self.i += 4;
                        }
                        _ => return None,
                    }
                    self.i += 1;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Value> {
        self.i += 1; // [
        let mut out = Vec::new();
        self.ws();
        if self.peek() == b']' {
            self.i += 1;
            return Some(Value::Arr(out));
        }
        loop {
            out.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                b',' => {
                    self.i += 1;
                }
                b']' => {
                    self.i += 1;
                    return Some(Value::Arr(out));
                }
                _ => return None,
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<Value> {
        self.i += 1; // {
        let mut out = BTreeMap::new();
        self.ws();
        if self.peek() == b'}' {
            self.i += 1;
            return Some(Value::Obj(out));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.peek() != b':' {
                return None;
            }
            self.i += 1;
            let val = self.value(depth + 1)?;
            out.insert(key, val);
            self.ws();
            match self.peek() {
                b',' => {
                    self.i += 1;
                }
                b'}' => {
                    self.i += 1;
                    return Some(Value::Obj(out));
                }
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_shape() {
        let j = br#"[{"Id":"abc123","Names":["/web"],"Image":"nginx","State":"running",
            "Status":"Up 3 hours","Ports":[{"PrivatePort":80,"PublicPort":8080,"Type":"tcp"}]}]"#;
        let v = parse(j).unwrap();
        let c = &v.arr()[0];
        assert_eq!(c.get("Id").str(), "abc123");
        assert_eq!(c.get("Names").arr()[0].str(), "/web");
        assert_eq!(c.get("State").str(), "running");
        let port = &c.get("Ports").arr()[0];
        assert_eq!(port.get("PublicPort").num() as u16, 8080);
    }

    #[test]
    fn rejects_junk_but_tolerates_trailing() {
        assert!(parse(b"{").is_none());
        assert!(parse(b"[1,2,]").is_none());
        assert_eq!(parse(b"true\n").unwrap(), Value::Bool(true));
    }
}
