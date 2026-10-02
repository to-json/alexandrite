//! Spans, source files and diagnostics.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub file: u32,
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn to(self, other: Span) -> Span {
        Span { file: self.file, lo: self.lo.min(other.lo), hi: self.hi.max(other.hi) }
    }
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub span: Span,
    pub msg: String,
    pub notes: Vec<String>,
}

impl Diag {
    pub fn new(span: Span, msg: impl Into<String>) -> Self {
        Diag { span, msg: msg.into(), notes: vec![] }
    }
    pub fn note(mut self, n: impl Into<String>) -> Self {
        self.notes.push(n.into());
        self
    }
}

pub struct SourceFile {
    /// Path as the user wrote it / as it should appear in messages.
    pub name: String,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(name: String, text: String) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile { name, text, line_starts }
    }

    /// 1-based line and column (column counts characters).
    pub fn line_col(&self, off: u32) -> (u32, u32) {
        let line = match self.line_starts.binary_search(&off) {
            Ok(l) => l,
            Err(l) => l - 1,
        };
        let start = self.line_starts[line] as usize;
        let col = self.text[start..off as usize].chars().count() as u32 + 1;
        (line as u32 + 1, col)
    }
}

#[derive(Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn add(&mut self, name: String, text: String) -> u32 {
        self.files.push(SourceFile::new(name, text));
        self.files.len() as u32 - 1
    }

    pub fn loc(&self, sp: Span) -> String {
        let f = &self.files[sp.file as usize];
        let (l, c) = f.line_col(sp.lo);
        format!("{}:{}:{}", f.name, l, c)
    }

    pub fn snippet(&self, sp: Span) -> &str {
        &self.files[sp.file as usize].text[sp.lo as usize..sp.hi as usize]
    }

    pub fn render_warning(&self, d: &Diag) -> String {
        let s = self.render(d);
        format!("warning{}", s.strip_prefix("error").unwrap_or(&s))
    }

    pub fn render(&self, d: &Diag) -> String {
        let f = &self.files[d.span.file as usize];
        let (l, c) = f.line_col(d.span.lo);
        let line_text = f.text.lines().nth(l as usize - 1).unwrap_or("");
        let mut s = format!("error: {}\n  --> {}:{}:{}\n   |\n{:>3}| {}\n   | {}^\n", d.msg, f.name, l, c, l, line_text, " ".repeat(c as usize - 1));
        for n in &d.notes {
            s.push_str(&format!("   = {n}\n"));
        }
        s
    }
}
