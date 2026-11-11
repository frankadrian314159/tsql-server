//! The XML behind the `xml` methods `.value()`, `.exist()`, `.query()`, `.nodes()` and `.modify()`.
//!
//! A document is parsed into an arena of nodes (elements, attributes of elements, text, comments, processing instructions);
//! names keep their prefixes and `xmlns` attributes stay where they are, so a document written out again is the document that
//! went in. Names are *matched* by their local part: namespaces are not resolved, and a `declare namespace ...;` or `declare
//! default element namespace ...;` prolog is accepted and not used.
//!
//! The query language is a subset of XQuery 1.0 (XPath 2.0 without types):
//!
//! * paths: absolute (`/a/b`, `//b`) and relative (`a/b`, `./a`, `..`), the steps `name`, `p:name`, `*`, `@name`, `@*`,
//!   `text()`, `node()`, `.`; `(expr)[n]` followed by further steps, as in `(a)[1]/b`;
//! * predicates: `[n]`, `[last()]`, comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`) joined by `and` / `or`, `not(...)`, and a
//!   path as an existence test (`[@a]`, `[b]`);
//! * expressions: numbers, string literals, `+ - * div mod`, `( , )` sequences, and the functions `data`, `string`, `number`,
//!   `count`, `sum`, `not`, `true`, `false`, `concat`, `contains`, `starts-with`, `string-length`, `position`, `last`,
//!   `empty`, `exists`, and the casts `xs:string`, `xs:integer`, `xs:decimal`, `xs:double`; `sql:arg(n)` stands for a
//!   `sql:column()` or `sql:variable()` that the SQL parser has replaced with the number of an argument;
//! * `modify`: `insert <element/> | attribute n {e} | "text"  (as first | as last)? into | before | after target`,
//!   `delete target` and `replace value of target with expr`, with `{ expr }` inside the new content.
//!
//! Not supported: FLWOR (`for`, `let`, `where`, `return`), `if`, sequences of types, typed XML, axes written out
//! (`child::`), user functions, `fn:id` and the like.
//!
//! **Context of a relative path.** On a column or variable a relative path starts at the document, as in SQL Server. The nodes
//! that `.nodes()` returns are handed on as xml text, and a relative path in a method of such a node starts at the node
//! itself (`ref.value('(Edu.Level)[1]', ...)`). To tell the two apart without a marker in the text, a relative path that finds
//! nothing at the document is tried again at the single top-level element, if there is one. The one case in which this
//! differs from SQL Server is a relative path that SQL Server would evaluate to nothing.
use std::fmt::Write as _;

#[derive(Clone, Debug)]
enum Kind {
    Doc,
    Elem { name: String, attrs: Vec<(String, String)> },
    Text(String),
    Comment(String),
    Pi(String),
}

#[derive(Clone, Debug)]
struct N {
    kind: Kind,
    parent: Option<usize>,
    kids: Vec<usize>,
}

/// A parsed document; node 0 is the document node.
#[derive(Clone, Debug)]
pub struct Doc {
    n: Vec<N>,
}

/// Something a query returns.
#[derive(Clone, Debug)]
pub enum Item {
    Node(usize),
    /// (owner element, name, value)
    Attr(usize, String, String),
    Atom(Atom),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Atom {
    Str(String),
    Num(f64),
    Bool(bool),
}

/// The value of a `sql:column()` or `sql:variable()`.
#[derive(Clone, Debug)]
pub enum Arg {
    Null,
    Str(String),
    Num(f64),
}

type Seq = Vec<Item>;
type Res<T> = Result<T, String>;

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let ent = &rest[1..end];
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => out.extend(u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)),
            e if e.starts_with('#') => out.extend(e[1..].parse::<u32>().ok().and_then(char::from_u32)),
            _ => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn esc(s: &str, attr: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' if attr => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

fn fmt_num(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "INF".into() } else { "-INF".into() }
    } else if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{}", f)
    }
}

impl Doc {
    /// Parse an XML document or fragment (several top-level nodes are allowed). `None` if it is not well-formed.
    pub fn parse(src: &str) -> Option<Doc> {
        let mut d = Doc { n: vec![N { kind: Kind::Doc, parent: None, kids: vec![] }] };
        let b = src.as_bytes();
        let mut i = 0;
        let mut stack: Vec<usize> = vec![0];
        while i < b.len() {
            let cur = *stack.last()?;
            if b[i] != b'<' {
                let end = src[i..].find('<').map(|k| i + k).unwrap_or(b.len());
                let t = &src[i..end];
                if !t.trim().is_empty() || stack.len() > 1 {
                    d.add(cur, Kind::Text(unescape(t)));
                }
                i = end;
                continue;
            }
            let rest = &src[i..];
            if rest.starts_with("<!--") {
                let end = rest.find("-->")?;
                d.add(cur, Kind::Comment(rest[4..end].to_string()));
                i += end + 3;
            } else if rest.starts_with("<![CDATA[") {
                let end = rest.find("]]>")?;
                d.add(cur, Kind::Text(rest[9..end].to_string()));
                i += end + 3;
            } else if rest.starts_with("<?") {
                let end = rest.find("?>")?;
                let body = &rest[2..end];
                // the XML declaration is not part of the document
                if !body.starts_with("xml ") && body != "xml" {
                    d.add(cur, Kind::Pi(body.to_string()));
                }
                i += end + 2;
            } else if rest.starts_with("<!") {
                i += rest.find('>')? + 1;
            } else if rest.starts_with("</") {
                let end = rest.find('>')?;
                let name = rest[2..end].trim();
                let id = stack.pop()?;
                if id == 0 {
                    return None;
                }
                match &d.n[id].kind {
                    Kind::Elem { name: n, .. } if n == name => {}
                    _ => return None,
                }
                i += end + 1;
            } else {
                let mut j = i + 1;
                let mut in_q: Option<u8> = None;
                while j < b.len() {
                    match (in_q, b[j]) {
                        (None, b'"') | (None, b'\'') => in_q = Some(b[j]),
                        (Some(q), c) if c == q => in_q = None,
                        (None, b'>') => break,
                        _ => {}
                    }
                    j += 1;
                }
                if j >= b.len() {
                    return None;
                }
                let mut inner = &src[i + 1..j];
                let empty = inner.ends_with('/');
                if empty {
                    inner = &inner[..inner.len() - 1];
                }
                let inner = inner.trim();
                let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
                let name = inner[..name_end].to_string();
                if name.is_empty() {
                    return None;
                }
                let mut attrs = vec![];
                let mut a = inner[name_end..].trim_start();
                while !a.is_empty() {
                    let eq = a.find('=')?;
                    let an = a[..eq].trim().to_string();
                    let v = a[eq + 1..].trim_start();
                    let q = v.chars().next()?;
                    if q != '"' && q != '\'' {
                        return None;
                    }
                    let close = v[1..].find(q)? + 1;
                    attrs.push((an, unescape(&v[1..close])));
                    a = v[close + 1..].trim_start();
                }
                let id = d.add(cur, Kind::Elem { name, attrs });
                if !empty {
                    stack.push(id);
                }
                i = j + 1;
            }
        }
        if stack.len() == 1 {
            Some(d)
        } else {
            None
        }
    }

    fn add(&mut self, parent: usize, kind: Kind) -> usize {
        let id = self.n.len();
        self.n.push(N { kind, parent: Some(parent), kids: vec![] });
        self.n[parent].kids.push(id);
        id
    }

    fn is_elem(&self, id: usize) -> bool {
        matches!(self.n[id].kind, Kind::Elem { .. })
    }

    /// The document as text.
    pub fn to_xml(&self) -> String {
        let mut o = String::new();
        for &k in &self.n[0].kids {
            self.ser(k, &mut o, &[]);
        }
        o
    }

    fn ser(&self, id: usize, o: &mut String, inherited: &[(String, String)]) {
        match &self.n[id].kind {
            Kind::Doc => {
                for &k in &self.n[id].kids {
                    self.ser(k, o, &[]);
                }
            }
            Kind::Text(t) => o.push_str(&esc(t, false)),
            Kind::Comment(c) => {
                let _ = write!(o, "<!--{}-->", c);
            }
            Kind::Pi(p) => {
                let _ = write!(o, "<?{}?>", p);
            }
            Kind::Elem { name, attrs } => {
                let _ = write!(o, "<{}", name);
                for (k, v) in inherited.iter().chain(attrs.iter()) {
                    let _ = write!(o, " {}=\"{}\"", k, esc(v, true));
                }
                if self.n[id].kids.is_empty() {
                    o.push_str("/>");
                } else {
                    o.push('>');
                    for &k in &self.n[id].kids {
                        self.ser(k, o, &[]);
                    }
                    let _ = write!(o, "</{}>", name);
                }
            }
        }
    }

    /// A node written out on its own: an element carries the namespace declarations that are in scope for it.
    fn fragment(&self, id: usize) -> String {
        let mut o = String::new();
        let mut inherited: Vec<(String, String)> = vec![];
        if let Kind::Elem { attrs, .. } = &self.n[id].kind {
            let mut up = self.n[id].parent;
            while let Some(p) = up {
                if let Kind::Elem { attrs: pa, .. } = &self.n[p].kind {
                    for (k, v) in pa {
                        if (k == "xmlns" || k.starts_with("xmlns:")) && !attrs.iter().any(|(a, _)| a == k) && !inherited.iter().any(|(a, _)| a == k) {
                            inherited.push((k.clone(), v.clone()));
                        }
                    }
                }
                up = self.n[p].parent;
            }
        }
        self.ser(id, &mut o, &inherited);
        o
    }

    fn string_of_node(&self, id: usize) -> String {
        match &self.n[id].kind {
            Kind::Text(t) => t.clone(),
            Kind::Comment(c) => c.clone(),
            Kind::Pi(p) => p.clone(),
            _ => self.n[id].kids.iter().filter(|&&k| !matches!(self.n[k].kind, Kind::Comment(_) | Kind::Pi(_))).map(|&k| self.string_of_node(k)).collect(),
        }
    }

    /// The string value of an item.
    pub fn string_of(&self, it: &Item) -> String {
        match it {
            Item::Node(id) => self.string_of_node(*id),
            Item::Attr(_, _, v) => v.clone(),
            Item::Atom(Atom::Str(s)) => s.clone(),
            Item::Atom(Atom::Num(f)) => fmt_num(*f),
            Item::Atom(Atom::Bool(b)) => (if *b { "true" } else { "false" }).to_string(),
        }
    }

    /// An item as xml text (what `.query()` returns).
    pub fn xml_of(&self, it: &Item) -> String {
        match it {
            Item::Node(id) => self.fragment(*id),
            other => esc(&self.string_of(other), false),
        }
    }

    fn single_root(&self) -> Option<usize> {
        let mut es = self.n[0].kids.iter().filter(|&&k| self.is_elem(k));
        let first = *es.next()?;
        if es.next().is_some() {
            None
        } else {
            Some(first)
        }
    }

    // ------------------------------------------------------------ queries

    /// Evaluate a query. `Err` is a malformed or unsupported query.
    pub fn select(&self, query: &str, args: &[Arg]) -> Res<Vec<Item>> {
        let e = parse_query(query)?;
        let ev = Ev { doc: self, args };
        let focus = Focus { item: Some(Item::Node(0)), pos: 1, size: 1 };
        let r = ev.eval(&e, &focus)?;
        if r.is_empty() && is_relative(&e) {
            if let Some(root) = self.single_root() {
                return ev.eval(&e, &Focus { item: Some(Item::Node(root)), pos: 1, size: 1 });
            }
        }
        Ok(r)
    }

    /// The nodes a path selects, each as the xml text of an element (what `.nodes()` hands out).
    pub fn nodes(&self, query: &str, args: &[Arg]) -> Res<Vec<String>> {
        let items = self.select(query, args)?;
        items
            .iter()
            .map(|i| match i {
                Item::Node(id) if self.is_elem(*id) || *id == 0 => Ok(self.fragment(*id)),
                _ => Err("the path of nodes() must select elements".to_string()),
            })
            .collect()
    }

    // ------------------------------------------------------------ modify

    /// Apply an XML data manipulation statement: `insert`, `delete` or `replace value of`.
    pub fn modify(&mut self, stmt: &str, args: &[Arg]) -> Res<()> {
        let s = strip_prolog(stmt)?;
        let s = s.trim();
        if let Some(rest) = kw(s, "insert") {
            self.m_insert(rest.trim_start(), args)
        } else if let Some(rest) = kw(s, "delete") {
            let e = parse_query(rest)?;
            let items = Ev { doc: self, args }.eval(&e, &Focus { item: Some(Item::Node(0)), pos: 1, size: 1 })?;
            for it in items {
                match it {
                    Item::Node(id) if id != 0 => self.unlink(id),
                    Item::Attr(owner, name, _) => {
                        if let Kind::Elem { attrs, .. } = &mut self.n[owner].kind {
                            attrs.retain(|(k, _)| local(k) != local(&name));
                        }
                    }
                    _ => return Err("the target of 'delete' must be an element, an attribute or text".into()),
                }
            }
            Ok(())
        } else if let Some(rest) = kw(s, "replace") {
            let rest = kw(rest.trim_start(), "value").ok_or("expected 'replace value of'")?;
            let rest = kw(rest.trim_start(), "of").ok_or("expected 'replace value of'")?;
            let at = find_word(rest, "with").ok_or("'replace value of' needs 'with'")?;
            let target = parse_query(&rest[..at])?;
            let with = parse_query(&rest[at + 4..])?;
            let ev = Ev { doc: self, args };
            let focus = Focus { item: Some(Item::Node(0)), pos: 1, size: 1 };
            let targets = ev.eval(&target, &focus)?;
            if targets.len() != 1 {
                return Err("the target of 'replace value of' must be a single element, attribute or text node".into());
            }
            let val = ev.eval(&with, &focus)?;
            if val.len() > 1 {
                return Err("the new value of 'replace value of' must be a single value".into());
            }
            let text = val.first().map(|i| self.string_of(i)).unwrap_or_default();
            match targets.into_iter().next().unwrap() {
                Item::Node(id) => match self.n[id].kind {
                    Kind::Elem { .. } => {
                        for k in std::mem::take(&mut self.n[id].kids) {
                            self.n[k].parent = None;
                        }
                        if !text.is_empty() {
                            self.add(id, Kind::Text(text));
                        }
                    }
                    Kind::Text(_) => self.n[id].kind = Kind::Text(text),
                    _ => return Err("the target of 'replace value of' must be an element, an attribute or text".into()),
                },
                Item::Attr(owner, name, _) => {
                    if let Kind::Elem { attrs, .. } = &mut self.n[owner].kind {
                        for a in attrs.iter_mut().filter(|(k, _)| local(k) == local(&name)) {
                            a.1 = text.clone();
                        }
                    }
                }
                Item::Atom(_) => return Err("the target of 'replace value of' must be a node".into()),
            }
            Ok(())
        } else {
            Err("modify() takes insert, delete or replace value of".into())
        }
    }

    fn unlink(&mut self, id: usize) {
        if let Some(p) = self.n[id].parent.take() {
            self.n[p].kids.retain(|&k| k != id);
        }
    }

    fn m_insert(&mut self, s: &str, args: &[Arg]) -> Res<()> {
        // what to insert
        enum What {
            Nodes(Doc),
            Attr(String, String),
        }
        let (what, rest) = if s.starts_with('<') {
            let end = element_end(s).ok_or("the element to insert is not well-formed")?;
            let content = self.expand_enclosed(&s[..end], args)?;
            let d = Doc::parse(&content).ok_or("the element to insert is not well-formed")?;
            (What::Nodes(d), &s[end..])
        } else if let Some(r) = kw(s, "attribute") {
            let r = r.trim_start();
            let name_end = r.find(|c: char| c.is_whitespace() || c == '{').ok_or("expected 'attribute name { value }'")?;
            let name = r[..name_end].to_string();
            let r = r[name_end..].trim_start().strip_prefix('{').ok_or("expected '{' after the attribute name")?;
            let close = matching_brace(r).ok_or("the value of the attribute is not closed")?;
            let e = parse_query(&r[..close])?;
            let ev = Ev { doc: self, args };
            let v = ev.eval(&e, &Focus { item: Some(Item::Node(0)), pos: 1, size: 1 })?;
            (What::Attr(name, v.iter().map(|i| self.string_of(i)).collect::<Vec<_>>().join(" ")), &r[close + 1..])
        } else if s.starts_with('"') || s.starts_with('\'') {
            let q = s.chars().next().unwrap();
            let close = s[1..].find(q).ok_or("the text to insert is not closed")? + 1;
            let mut d = Doc::parse("").unwrap();
            d.add(0, Kind::Text(s[1..close].to_string()));
            (What::Nodes(d), &s[close + 1..])
        } else {
            return Err("insert takes an element, an attribute or a string".into());
        };
        let rest = rest.trim_start();
        // where
        let (pos, rest) = if let Some(r) = kw(rest, "as") {
            let r = r.trim_start();
            let (p, r) = if let Some(r) = kw(r, "first") { ("first", r) } else if let Some(r) = kw(r, "last") { ("last", r) } else { return Err("expected 'as first' or 'as last'".into()) };
            ("as ".to_string() + p, kw(r.trim_start(), "into").ok_or("expected 'into'")?)
        } else if let Some(r) = kw(rest, "into") {
            ("into".to_string(), r)
        } else if let Some(r) = kw(rest, "after") {
            ("after".to_string(), r)
        } else if let Some(r) = kw(rest, "before") {
            ("before".to_string(), r)
        } else {
            return Err("expected 'into', 'as first into', 'as last into', 'before' or 'after'".into());
        };
        let target = parse_query(rest)?;
        let targets = Ev { doc: self, args }.eval(&target, &Focus { item: Some(Item::Node(0)), pos: 1, size: 1 })?;
        let t = match targets.as_slice() {
            [Item::Node(id)] => *id,
            _ => return Err("the target of 'insert' must be a single node".into()),
        };
        match what {
            What::Attr(name, value) => {
                if pos == "after" || pos == "before" {
                    return Err("an attribute can only be inserted 'into' an element".into());
                }
                match &mut self.n[t].kind {
                    Kind::Elem { attrs, .. } => {
                        if attrs.iter().any(|(k, _)| k == &name) {
                            return Err(format!("the attribute '{}' already exists", name));
                        }
                        attrs.push((name, value));
                    }
                    _ => return Err("an attribute can only be inserted into an element".into()),
                }
            }
            What::Nodes(src) => {
                let (parent, at) = match pos.as_str() {
                    "after" | "before" => {
                        let p = self.n[t].parent.ok_or("the target of 'before' or 'after' has no parent")?;
                        let idx = self.n[p].kids.iter().position(|&k| k == t).unwrap_or(0);
                        (p, if pos == "after" { idx + 1 } else { idx })
                    }
                    other => {
                        if !self.is_elem(t) && t != 0 {
                            return Err("the target of 'insert into' must be an element".into());
                        }
                        (t, if other == "as first" { 0 } else { self.n[t].kids.len() })
                    }
                };
                let mut at = at;
                for &k in &src.n[0].kids.clone() {
                    let id = self.graft(&src, k, parent);
                    // graft appended the node last: move it to its place
                    let last = self.n[parent].kids.pop();
                    debug_assert_eq!(last, Some(id));
                    self.n[parent].kids.insert(at, id);
                    at += 1;
                }
            }
        }
        Ok(())
    }

    /// Copy a node of another document below `parent` (appended).
    fn graft(&mut self, src: &Doc, id: usize, parent: usize) -> usize {
        let new = self.add(parent, src.n[id].kind.clone());
        for &k in &src.n[id].kids {
            self.graft(src, k, new);
        }
        new
    }

    /// Replace `{ expr }` in new content by the value of the expression (`{{` is a brace).
    fn expand_enclosed(&self, content: &str, args: &[Arg]) -> Res<String> {
        if !content.contains('{') {
            return Ok(content.to_string());
        }
        let mut out = String::new();
        let mut rest = content;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            rest = &rest[i + 1..];
            if let Some(r) = rest.strip_prefix('{') {
                out.push('{');
                rest = r;
                continue;
            }
            let close = matching_brace(rest).ok_or("an enclosed expression is not closed")?;
            let e = parse_query(&rest[..close])?;
            let v = Ev { doc: self, args }.eval(&e, &Focus { item: Some(Item::Node(0)), pos: 1, size: 1 })?;
            out.push_str(&esc(&v.iter().map(|i| self.string_of(i)).collect::<Vec<_>>().join(" "), false));
            rest = &rest[close + 1..];
        }
        out.push_str(rest);
        Ok(out.replace("}}", "}"))
    }
}

/// `s` starts with the word `w` (followed by a non-name character): the rest.
fn kw<'a>(s: &'a str, w: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(w)?;
    match rest.chars().next() {
        Some(c) if c.is_alphanumeric() || c == '_' || c == '-' => None,
        _ => Some(rest),
    }
}

/// The byte index of the word `w` outside quotes and brackets.
fn find_word(s: &str, w: &str) -> Option<usize> {
    let b = s.as_bytes();
    let (mut depth, mut q) = (0i32, None);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match q {
            Some(qq) => {
                if c == qq {
                    q = None;
                }
            }
            None => match c {
                b'"' | b'\'' => q = Some(c),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ if depth == 0 && s[i..].starts_with(w) && (i == 0 || !(b[i - 1] as char).is_alphanumeric()) && kw(&s[i..], w).is_some() => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// The byte index just after the element that starts `s`.
fn element_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let (mut i, mut depth) = (0, 0);
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &s[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->")? + 3;
        } else if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>")? + 3;
        } else if rest.starts_with("</") {
            i += rest.find('>')? + 1;
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        } else {
            let mut j = i + 1;
            let mut q: Option<u8> = None;
            while j < b.len() {
                match (q, b[j]) {
                    (None, b'"') | (None, b'\'') => q = Some(b[j]),
                    (Some(qq), c) if c == qq => q = None,
                    (None, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            let selfclosing = j > 0 && b.get(j - 1) == Some(&b'/');
            i = j + 1;
            if !selfclosing {
                depth += 1;
            } else if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// The index of the `}` that closes an enclosed expression (the text after its `{`).
fn matching_brace(s: &str) -> Option<usize> {
    let (mut depth, mut q) = (1, None);
    for (i, c) in s.char_indices() {
        match (q, c) {
            (Some(qq), c) if c == qq => q = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => q = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove `declare namespace ...;` and `declare default element namespace ...;` from the start of a query.
fn strip_prolog(q: &str) -> Res<&str> {
    let mut s = q.trim_start();
    while let Some(r) = kw(s, "declare") {
        let end = r.find(';').ok_or("a declaration in the prolog is not ended with ';'")?;
        let decl = r[..end].trim_start();
        if !(decl.starts_with("namespace") || decl.starts_with("default")) {
            return Err(format!("the declaration '{}' is not supported", decl.split_whitespace().next().unwrap_or("")));
        }
        s = r[end + 1..].trim_start();
    }
    Ok(s)
}

// ---------------------------------------------------------------- the query language

#[derive(Debug, Clone)]
enum E {
    Num(f64),
    Str(String),
    Path(Box<PathE>),
    Func(String, Vec<E>),
    Bin(&'static str, Box<E>, Box<E>),
    Neg(Box<E>),
    Seq(Vec<E>),
}

#[derive(Debug, Clone)]
struct PathE {
    start: Start,
    steps: Vec<Step>,
}

#[derive(Debug, Clone)]
enum Start {
    Root,
    Ctx,
    /// a parenthesised expression, a function call or a literal, with predicates
    Primary(E, Vec<E>),
}

#[derive(Debug, Clone)]
struct Step {
    /// written with `//`
    desc: bool,
    test: Test,
    preds: Vec<E>,
}

#[derive(Debug, Clone)]
enum Test {
    Name(String),
    Any,
    Attr(String),
    Text,
    AnyNode,
    SelfNode,
    Parent,
}

struct P<'a> {
    s: &'a str,
    i: usize,
}

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')
}

fn parse_query(q: &str) -> Res<E> {
    let body = strip_prolog(q)?;
    let mut p = P { s: body, i: 0 };
    let e = p.expr()?;
    p.ws();
    if p.i < p.s.len() {
        return Err(format!("unexpected '{}' in the query", &p.s[p.i..].chars().take(12).collect::<String>()));
    }
    Ok(e)
}

fn is_relative(e: &E) -> bool {
    match e {
        E::Path(p) => match &p.start {
            Start::Ctx => true,
            Start::Primary(inner, _) => is_relative(inner),
            Start::Root => false,
        },
        E::Seq(v) => v.iter().all(is_relative),
        _ => false,
    }
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.s[self.i..].starts_with(|c: char| c.is_whitespace()) {
            self.i += self.s[self.i..].chars().next().unwrap().len_utf8();
        }
        // (: comments :)
        while self.s[self.i..].starts_with("(:") {
            match self.s[self.i..].find(":)") {
                Some(e) => self.i += e + 2,
                None => self.i = self.s.len(),
            }
            while self.s[self.i..].starts_with(|c: char| c.is_whitespace()) {
                self.i += 1;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        if self.s[self.i..].starts_with(t) {
            self.i += t.len();
            true
        } else {
            false
        }
    }

    /// The word `w` (not followed by a name character).
    fn eat_word(&mut self, w: &str) -> bool {
        self.ws();
        match kw(&self.s[self.i..], w) {
            Some(_) => {
                self.i += w.len();
                true
            }
            None => false,
        }
    }

    fn expr(&mut self) -> Res<E> {
        let first = self.or_expr()?;
        self.ws();
        if self.peek() == Some(',') {
            let mut v = vec![first];
            while self.eat(",") {
                v.push(self.or_expr()?);
            }
            return Ok(E::Seq(v));
        }
        Ok(first)
    }

    fn or_expr(&mut self) -> Res<E> {
        let mut l = self.and_expr()?;
        while self.eat_word("or") {
            let r = self.and_expr()?;
            l = E::Bin("or", Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Res<E> {
        let mut l = self.cmp_expr()?;
        while self.eat_word("and") {
            let r = self.cmp_expr()?;
            l = E::Bin("and", Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn cmp_expr(&mut self) -> Res<E> {
        let l = self.add_expr()?;
        self.ws();
        for (t, name) in [("!=", "!="), ("<=", "<="), (">=", ">="), ("=", "="), ("<", "<"), (">", ">")] {
            if self.s[self.i..].starts_with(t) {
                self.i += t.len();
                let r = self.add_expr()?;
                return Ok(E::Bin(name, Box::new(l), Box::new(r)));
            }
        }
        Ok(l)
    }

    fn add_expr(&mut self) -> Res<E> {
        let mut l = self.mul_expr()?;
        loop {
            self.ws();
            let op = match self.peek() {
                Some('+') => "+",
                Some('-') => "-",
                _ => break,
            };
            self.i += 1;
            let r = self.mul_expr()?;
            l = E::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn mul_expr(&mut self) -> Res<E> {
        let mut l = self.unary()?;
        loop {
            self.ws();
            let op = if self.peek() == Some('*') {
                self.i += 1;
                "*"
            } else if self.eat_word("div") {
                "div"
            } else if self.eat_word("idiv") {
                "idiv"
            } else if self.eat_word("mod") {
                "mod"
            } else {
                break;
            };
            let r = self.unary()?;
            l = E::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self) -> Res<E> {
        self.ws();
        if self.peek() == Some('-') {
            self.i += 1;
            return Ok(E::Neg(Box::new(self.unary()?)));
        }
        if self.peek() == Some('+') {
            self.i += 1;
            return self.unary();
        }
        self.path()
    }

    fn name(&mut self) -> Option<String> {
        let start = self.i;
        let mut it = self.s[self.i..].char_indices();
        let (_, first) = it.next()?;
        if !(first.is_alphabetic() || first == '_' || first == '*') {
            return None;
        }
        if first == '*' {
            self.i += 1;
            return Some("*".into());
        }
        let mut end = self.s.len();
        for (k, c) in self.s[self.i..].char_indices() {
            if !name_char(c) {
                end = self.i + k;
                break;
            }
        }
        // a prefix wildcard p:*
        if self.s[end..].starts_with('*') && self.s[start..end].ends_with(':') {
            end += 1;
        }
        // a name does not end in '-' or '.' followed by a space: `a - b`
        let mut e = end;
        while e > start + 1 && matches!(self.s.as_bytes()[e - 1], b'-' | b'.') && self.s[e..].starts_with(|c: char| c.is_whitespace()) {
            e -= 1;
        }
        self.i = e;
        Some(self.s[start..e].to_string())
    }

    fn preds(&mut self) -> Res<Vec<E>> {
        let mut v = vec![];
        loop {
            self.ws();
            if self.peek() != Some('[') {
                break;
            }
            self.i += 1;
            v.push(self.expr()?);
            if !self.eat("]") {
                return Err("a predicate is not closed with ']'".into());
            }
        }
        Ok(v)
    }

    fn step(&mut self, desc: bool) -> Res<Step> {
        self.ws();
        let test = if self.s[self.i..].starts_with("..") {
            self.i += 2;
            Test::Parent
        } else if self.s[self.i..].starts_with('.') && !self.s[self.i + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            self.i += 1;
            Test::SelfNode
        } else if self.s[self.i..].starts_with('@') {
            self.i += 1;
            Test::Attr(self.name().ok_or("an attribute name is expected after '@'")?)
        } else {
            let n = self.name().ok_or_else(|| format!("a step is expected at '{}'", self.s[self.i..].chars().take(12).collect::<String>()))?;
            let after = self.i;
            self.ws();
            if self.s[self.i..].starts_with('(') && (n == "text" || n == "node") {
                self.i += 1;
                if !self.eat(")") {
                    return Err(format!("{}() takes no arguments", n));
                }
                if n == "text" { Test::Text } else { Test::AnyNode }
            } else {
                self.i = after;
                if n == "*" || n.ends_with(":*") { Test::Any } else { Test::Name(n) }
            }
        };
        let preds = self.preds()?;
        Ok(Step { desc, test, preds })
    }

    fn path(&mut self) -> Res<E> {
        self.ws();
        let c = self.peek().ok_or("the query ends where an expression is expected")?;
        let mut steps = vec![];
        let start;
        if c == '/' {
            start = Start::Root;
            if self.s[self.i..].starts_with("//") {
                self.i += 2;
                steps.push(self.step(true)?);
            } else {
                self.i += 1;
                // a lone '/' is the document
                self.ws();
                if self.i < self.s.len() && (self.peek().map(|c| c.is_alphabetic() || c == '_' || c == '*' || c == '@' || c == '.').unwrap_or(false)) {
                    steps.push(self.step(false)?);
                }
            }
        } else if c == '(' || c == '"' || c == '\'' || c.is_ascii_digit() || (c == '.' && self.s[self.i + 1..].starts_with(|d: char| d.is_ascii_digit())) {
            let prim = self.primary()?;
            let preds = self.preds()?;
            if preds.is_empty() && !matches!(self.peek_after_ws(), Some('/')) {
                return Ok(prim);
            }
            start = Start::Primary(prim, preds);
        } else if c.is_alphabetic() || c == '_' {
            // a function call, or a relative path
            let save = self.i;
            let n = self.name().ok_or("a name is expected")?;
            self.ws();
            let is_call = self.peek() == Some('(') && n != "text" && n != "node";
            if is_call {
                self.i = save;
                let prim = self.primary()?;
                let preds = self.preds()?;
                if preds.is_empty() && !matches!(self.peek_after_ws(), Some('/')) {
                    return Ok(prim);
                }
                start = Start::Primary(prim, preds);
            } else {
                self.i = save;
                start = Start::Ctx;
                steps.push(self.step(false)?);
            }
        } else if c == '.' || c == '@' || c == '*' {
            start = Start::Ctx;
            steps.push(self.step(false)?);
        } else {
            return Err(format!("unexpected '{}' in the query", c));
        }
        loop {
            self.ws();
            if self.s[self.i..].starts_with("//") {
                self.i += 2;
                steps.push(self.step(true)?);
            } else if self.s[self.i..].starts_with('/') {
                self.i += 1;
                steps.push(self.step(false)?);
            } else {
                break;
            }
        }
        Ok(E::Path(Box::new(PathE { start, steps })))
    }

    fn peek_after_ws(&mut self) -> Option<char> {
        self.ws();
        self.peek()
    }

    fn primary(&mut self) -> Res<E> {
        self.ws();
        let c = self.peek().ok_or("an expression is expected")?;
        if c == '(' {
            self.i += 1;
            self.ws();
            if self.peek() == Some(')') {
                self.i += 1;
                return Ok(E::Seq(vec![]));
            }
            let e = self.expr()?;
            if !self.eat(")") {
                return Err("a '(' is not closed".into());
            }
            return Ok(e);
        }
        if c == '"' || c == '\'' {
            self.i += 1;
            let mut out = String::new();
            loop {
                let rest = &self.s[self.i..];
                let k = rest.find(c).ok_or("a string literal is not closed")?;
                out.push_str(&rest[..k]);
                self.i += k + 1;
                if self.s[self.i..].starts_with(c) {
                    out.push(c);
                    self.i += 1;
                } else {
                    break;
                }
            }
            return Ok(E::Str(unescape(&out)));
        }
        if c.is_ascii_digit() || c == '.' {
            let start = self.i;
            while self.s[self.i..].starts_with(|d: char| d.is_ascii_digit() || d == '.') {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(['e', 'E']) && self.s[self.i + 1..].starts_with(|d: char| d.is_ascii_digit() || d == '-' || d == '+') {
                self.i += 2;
                while self.s[self.i..].starts_with(|d: char| d.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            return self.s[start..self.i].parse::<f64>().map(E::Num).map_err(|_| format!("'{}' is not a number", &self.s[start..self.i]));
        }
        // a function call
        let n = self.name().ok_or("an expression is expected")?;
        self.ws();
        if !self.s[self.i..].starts_with('(') {
            return Err(format!("'{}' is not a function call", n));
        }
        self.i += 1;
        let mut args = vec![];
        self.ws();
        if self.peek() != Some(')') {
            loop {
                args.push(self.or_expr()?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        if !self.eat(")") {
            return Err(format!("the call of {}() is not closed", n));
        }
        let n = n.strip_prefix("fn:").unwrap_or(&n).to_string();
        Ok(E::Func(n, args))
    }
}

// ---------------------------------------------------------------- evaluation

struct Focus {
    item: Option<Item>,
    pos: usize,
    size: usize,
}

struct Ev<'a> {
    doc: &'a Doc,
    args: &'a [Arg],
}

impl<'a> Ev<'a> {
    fn atomize(&self, s: &Seq) -> Vec<Atom> {
        s.iter()
            .map(|i| match i {
                Item::Atom(a) => a.clone(),
                other => Atom::Str(self.doc.string_of(other)),
            })
            .collect()
    }

    fn num_of(&self, a: &Atom) -> f64 {
        match a {
            Atom::Num(f) => *f,
            Atom::Bool(b) => *b as i32 as f64,
            Atom::Str(s) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
        }
    }

    fn ebv(&self, s: &Seq) -> bool {
        match s.first() {
            None => false,
            Some(Item::Node(_)) | Some(Item::Attr(..)) => true,
            Some(Item::Atom(Atom::Bool(b))) => *b,
            Some(Item::Atom(Atom::Num(f))) => *f != 0.0 && !f.is_nan(),
            Some(Item::Atom(Atom::Str(t))) => !t.is_empty(),
        }
    }

    fn descendants(&self, id: usize, out: &mut Vec<usize>) {
        for &k in &self.doc.n[id].kids {
            out.push(k);
            self.descendants(k, out);
        }
    }

    fn name_matches(&self, id: usize, test: &str) -> bool {
        match &self.doc.n[id].kind {
            Kind::Elem { name, .. } => test == "*" || local(name) == local(test),
            _ => false,
        }
    }

    /// The nodes (and attributes) a step selects from one node.
    fn axis(&self, id: usize, st: &Step) -> Seq {
        let d = self.doc;
        let mut pool: Vec<usize> = vec![];
        let mut out: Seq = vec![];
        match &st.test {
            Test::Name(n) => {
                if st.desc {
                    self.descendants(id, &mut pool);
                } else {
                    pool = d.n[id].kids.clone();
                }
                out.extend(pool.into_iter().filter(|&k| self.name_matches(k, n)).map(Item::Node));
            }
            Test::Any => {
                if st.desc {
                    self.descendants(id, &mut pool);
                } else {
                    pool = d.n[id].kids.clone();
                }
                out.extend(pool.into_iter().filter(|&k| d.is_elem(k)).map(Item::Node));
            }
            Test::Text => {
                if st.desc {
                    self.descendants(id, &mut pool);
                } else {
                    pool = d.n[id].kids.clone();
                }
                out.extend(pool.into_iter().filter(|&k| matches!(d.n[k].kind, Kind::Text(_))).map(Item::Node));
            }
            Test::AnyNode => {
                if st.desc {
                    self.descendants(id, &mut pool);
                } else {
                    pool = d.n[id].kids.clone();
                }
                out.extend(pool.into_iter().map(Item::Node));
            }
            Test::Attr(a) => {
                let mut owners = vec![id];
                if st.desc {
                    self.descendants(id, &mut owners);
                }
                for o in owners {
                    if let Kind::Elem { attrs, .. } = &d.n[o].kind {
                        for (k, v) in attrs {
                            if k == "xmlns" || k.starts_with("xmlns:") {
                                continue;
                            }
                            if a == "*" || local(k) == local(a) {
                                out.push(Item::Attr(o, k.clone(), v.clone()));
                            }
                        }
                    }
                }
            }
            Test::SelfNode => out.push(Item::Node(id)),
            Test::Parent => {
                if let Some(p) = d.n[id].parent {
                    out.push(Item::Node(p));
                }
            }
        }
        out
    }

    fn apply_preds(&self, items: Seq, preds: &[E]) -> Res<Seq> {
        let mut cur = items;
        for p in preds {
            let size = cur.len();
            let mut next = vec![];
            for (k, it) in cur.into_iter().enumerate() {
                let f = Focus { item: Some(it.clone()), pos: k + 1, size };
                let r = self.eval(p, &f)?;
                let keep = match r.as_slice() {
                    [Item::Atom(Atom::Num(n))] => (*n - (k + 1) as f64).abs() < 1e-9,
                    _ => self.ebv(&r),
                };
                if keep {
                    next.push(it);
                }
            }
            cur = next;
        }
        Ok(cur)
    }

    fn eval(&self, e: &E, f: &Focus) -> Res<Seq> {
        match e {
            E::Num(n) => Ok(vec![Item::Atom(Atom::Num(*n))]),
            E::Str(s) => Ok(vec![Item::Atom(Atom::Str(s.clone()))]),
            E::Neg(x) => {
                let a = self.atomize(&self.eval(x, f)?);
                Ok(a.first().map(|a| Item::Atom(Atom::Num(-self.num_of(a)))).into_iter().collect())
            }
            E::Seq(v) => {
                let mut out = vec![];
                for x in v {
                    out.extend(self.eval(x, f)?);
                }
                Ok(out)
            }
            E::Path(p) => self.eval_path(p, f),
            E::Bin(op, l, r) => self.eval_bin(op, l, r, f),
            E::Func(n, args) => self.eval_func(n, args, f),
        }
    }

    fn eval_path(&self, p: &PathE, f: &Focus) -> Res<Seq> {
        let mut cur: Seq = match &p.start {
            Start::Root => vec![Item::Node(0)],
            Start::Ctx => match &f.item {
                Some(i) => vec![i.clone()],
                None => return Err("a relative path needs a context item".into()),
            },
            Start::Primary(e, preds) => {
                let s = self.eval(e, f)?;
                self.apply_preds(s, preds)?
            }
        };
        for st in &p.steps {
            let mut next: Seq = vec![];
            let mut seen = std::collections::HashSet::new();
            for it in &cur {
                let Item::Node(id) = it else { return Err("a path step needs a node".into()) };
                let cands = self.axis(*id, st);
                let cands = self.apply_preds(cands, &st.preds)?;
                for c in cands {
                    if let Item::Node(n) = &c {
                        if !seen.insert(*n) {
                            continue;
                        }
                    }
                    next.push(c);
                }
            }
            cur = next;
        }
        Ok(cur)
    }

    fn eval_bin(&self, op: &str, l: &E, r: &E, f: &Focus) -> Res<Seq> {
        match op {
            "or" => {
                let a = self.ebv(&self.eval(l, f)?);
                Ok(vec![Item::Atom(Atom::Bool(a || self.ebv(&self.eval(r, f)?)))])
            }
            "and" => {
                let a = self.ebv(&self.eval(l, f)?);
                Ok(vec![Item::Atom(Atom::Bool(a && self.ebv(&self.eval(r, f)?)))])
            }
            "=" | "!=" | "<" | "<=" | ">" | ">=" => {
                let (a, b) = (self.atomize(&self.eval(l, f)?), self.atomize(&self.eval(r, f)?));
                let mut hit = false;
                'o: for x in &a {
                    for y in &b {
                        let ord = if matches!(x, Atom::Num(_)) || matches!(y, Atom::Num(_)) {
                            let (p, q) = (self.num_of(x), self.num_of(y));
                            if p.is_nan() || q.is_nan() {
                                continue;
                            }
                            p.partial_cmp(&q)
                        } else {
                            let (p, q) = (self.doc.string_of(&Item::Atom(x.clone())), self.doc.string_of(&Item::Atom(y.clone())));
                            Some(p.cmp(&q))
                        };
                        let Some(o) = ord else { continue };
                        use std::cmp::Ordering::*;
                        let ok = match op {
                            "=" => o == Equal,
                            "!=" => o != Equal,
                            "<" => o == Less,
                            "<=" => o != Greater,
                            ">" => o == Greater,
                            _ => o != Less,
                        };
                        if ok {
                            hit = true;
                            break 'o;
                        }
                    }
                }
                Ok(vec![Item::Atom(Atom::Bool(hit))])
            }
            _ => {
                let (a, b) = (self.atomize(&self.eval(l, f)?), self.atomize(&self.eval(r, f)?));
                let (Some(x), Some(y)) = (a.first(), b.first()) else { return Ok(vec![]) };
                if a.len() > 1 || b.len() > 1 {
                    return Err("an arithmetic operand must be a single value".into());
                }
                let (x, y) = (self.num_of(x), self.num_of(y));
                let v = match op {
                    "+" => x + y,
                    "-" => x - y,
                    "*" => x * y,
                    "div" => x / y,
                    "idiv" => (x / y).trunc(),
                    "mod" => x % y,
                    _ => return Err(format!("the operator {} is not supported", op)),
                };
                Ok(vec![Item::Atom(Atom::Num(v))])
            }
        }
    }

    fn eval_func(&self, name: &str, args: &[E], f: &Focus) -> Res<Seq> {
        let arg = |i: usize| -> Res<Seq> {
            match args.get(i) {
                Some(e) => self.eval(e, f),
                None => Ok(f.item.clone().into_iter().collect()),
            }
        };
        let b = |v: bool| Ok(vec![Item::Atom(Atom::Bool(v))]);
        let st = |s: String| Ok(vec![Item::Atom(Atom::Str(s))]);
        let first_string = |s: &Seq| s.first().map(|i| self.doc.string_of(i)).unwrap_or_default();
        match name {
            "sql:arg" => {
                let n = match args.first() {
                    Some(E::Num(n)) => *n as usize,
                    _ => return Err("sql:column() and sql:variable() take a literal name".into()),
                };
                Ok(match self.args.get(n) {
                    Some(Arg::Str(s)) => vec![Item::Atom(Atom::Str(s.clone()))],
                    Some(Arg::Num(x)) => vec![Item::Atom(Atom::Num(*x))],
                    _ => vec![],
                })
            }
            "sql:column" | "sql:variable" => Err("sql:column() and sql:variable() need the SQL parser to bind them".into()),
            "data" => Ok(self.atomize(&arg(0)?).into_iter().map(Item::Atom).collect()),
            "string" | "xs:string" => st(first_string(&arg(0)?)),
            "number" | "xs:double" | "xs:decimal" | "xs:integer" | "xs:int" | "xs:float" => {
                let a = self.atomize(&arg(0)?);
                Ok(a.first().map(|a| Item::Atom(Atom::Num(self.num_of(a)))).into_iter().collect())
            }
            "count" => Ok(vec![Item::Atom(Atom::Num(arg(0)?.len() as f64))]),
            "sum" => {
                let a = self.atomize(&arg(0)?);
                Ok(vec![Item::Atom(Atom::Num(a.iter().map(|x| self.num_of(x)).sum()))])
            }
            "not" => b(!self.ebv(&arg(0)?)),
            "true" => b(true),
            "false" => b(false),
            "empty" => b(arg(0)?.is_empty()),
            "exists" => b(!arg(0)?.is_empty()),
            "position" => Ok(vec![Item::Atom(Atom::Num(f.pos as f64))]),
            "last" => Ok(vec![Item::Atom(Atom::Num(f.size as f64))]),
            "concat" => {
                let mut o = String::new();
                for i in 0..args.len() {
                    o.push_str(&first_string(&arg(i)?));
                }
                st(o)
            }
            "contains" => b(first_string(&arg(0)?).contains(&first_string(&arg(1)?))),
            "starts-with" => b(first_string(&arg(0)?).starts_with(&first_string(&arg(1)?))),
            "string-length" => Ok(vec![Item::Atom(Atom::Num(first_string(&arg(0)?).chars().count() as f64))]),
            "upper-case" => st(first_string(&arg(0)?).to_uppercase()),
            "lower-case" => st(first_string(&arg(0)?).to_lowercase()),
            other => Err(format!("the function {}() is not supported", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Doc {
        Doc::parse(s).unwrap()
    }
    fn v(d: &Doc, q: &str) -> Option<String> {
        d.select(q, &[]).unwrap().first().map(|i| d.string_of(i))
    }

    #[test]
    fn paths() {
        let d = doc(r#"<?xml version="1.0"?><EVENT_INSTANCE><EventType>CREATE_TABLE</EventType><Obj id="7" kind="t"><Name>a&amp;b</Name></Obj><Obj id="8"><Name>c</Name></Obj></EVENT_INSTANCE>"#);
        assert_eq!(v(&d, "(/EVENT_INSTANCE/EventType)[1]").as_deref(), Some("CREATE_TABLE"));
        assert_eq!(v(&d, "/EVENT_INSTANCE/Obj[2]/Name").as_deref(), Some("c"));
        assert_eq!(v(&d, "(/EVENT_INSTANCE/Obj/@id)[2]").as_deref(), Some("8"));
        assert_eq!(v(&d, "/EVENT_INSTANCE/Obj[@id='7']/Name/text()").as_deref(), Some("a&b"));
        assert_eq!(v(&d, "//Name").as_deref(), Some("a&b"));
        assert_eq!(v(&d, "/EVENT_INSTANCE/Nope"), None);
        assert_eq!(d.select("/EVENT_INSTANCE/Obj", &[]).unwrap().len(), 2);
        let last = d.select("/EVENT_INSTANCE/Obj[last()]", &[]).unwrap();
        assert_eq!(d.xml_of(&last[0]), "<Obj id=\"8\"><Name>c</Name></Obj>");
        assert!(Doc::parse("<a><b></a>").is_none());
        // predicates with comparisons, boolean operators and existence tests
        assert_eq!(d.select("/EVENT_INSTANCE/Obj[@id > 7]", &[]).unwrap().len(), 1);
        assert_eq!(d.select("/EVENT_INSTANCE/Obj[@kind and Name]", &[]).unwrap().len(), 1);
        assert_eq!(d.select("/EVENT_INSTANCE/Obj[not(@kind)]/Name", &[]).unwrap().len(), 1);
        assert_eq!(d.select("/EVENT_INSTANCE/Obj[Name='c' or @id=7]", &[]).unwrap().len(), 2);
    }

    #[test]
    fn prefixes_are_matched_by_local_name_and_declarations_are_skipped() {
        let d = doc(r#"<ci:Info xmlns:ci="urn:ci" xmlns="urn:d"><act:tel xmlns:act="urn:act"><act:number>555</act:number></act:tel><b>2</b></ci:Info>"#);
        assert_eq!(v(&d, r#"declare namespace ci="urn:ci"; declare namespace act="urn:act"; (/ci:Info/act:tel/act:number)[1]"#).as_deref(), Some("555"));
        assert_eq!(v(&d, r#"declare default element namespace "urn:d"; /Info/b"#).as_deref(), Some("2"));
        // a selected element keeps the declarations that are in scope for it
        let r = d.select("/ci:Info/act:tel", &[]).unwrap();
        assert_eq!(d.xml_of(&r[0]), r#"<act:tel xmlns:ci="urn:ci" xmlns="urn:d" xmlns:act="urn:act"><act:number>555</act:number></act:tel>"#);
        // xmlns attributes are not attributes of a path
        assert!(d.select("/ci:Info/@*", &[]).unwrap().is_empty());
        // the document is written as it was
        assert_eq!(d.to_xml(), r#"<ci:Info xmlns:ci="urn:ci" xmlns="urn:d"><act:tel xmlns:act="urn:act"><act:number>555</act:number></act:tel><b>2</b></ci:Info>"#);
    }

    #[test]
    fn relative_paths_and_nodes() {
        let d = doc("<r><e id='1'><n>a</n><n>b</n></e><e id='2'><n>c</n></e></r>");
        let es = d.nodes("/r/e", &[]).unwrap();
        assert_eq!(es, vec!["<e id=\"1\"><n>a</n><n>b</n></e>", "<e id=\"2\"><n>c</n></e>"]);
        // the nodes are handed on as text, and a relative path in a method starts at the node
        let e1 = doc(&es[0]);
        assert_eq!(v(&e1, "(n)[2]").as_deref(), Some("b"));
        assert_eq!(v(&e1, "n[1]").as_deref(), Some("a"));
        assert_eq!(v(&e1, "@id[1]").as_deref(), Some("1"));
        assert_eq!(v(&e1, "string(.)[1]").as_deref(), Some("ab"));
        assert_eq!(e1.nodes("n", &[]).unwrap().len(), 2);
        // on a document a relative path starts at the document: /r/e/n is found by r/e/n
        assert_eq!(v(&d, "r/e/n").as_deref(), Some("a"));
        // a path after a parenthesised step
        assert_eq!(v(&d, "(/r/e)[2]/n").as_deref(), Some("c"));
        assert_eq!(v(&d, "(r/e)[1]/@id").as_deref(), Some("1"));
        assert_eq!(v(&d, "(/r/e/n/text())[3]").as_deref(), Some("c"));
        assert_eq!(v(&d, "count(/r/e/n)").as_deref(), Some("3"));
        assert_eq!(v(&d, "sum(/r/e/@id) * 2 + 1").as_deref(), Some("7"));
    }

    #[test]
    fn modify_inserts_deletes_and_replaces() {
        let mut d = doc(r#"<IndividualSurvey xmlns="urn:s"><Year>1999</Year><Total>10.5</Total></IndividualSurvey>"#);
        d.modify(r#"declare default element namespace "urn:s"; insert <TotalPurchaseYTD>0.00</TotalPurchaseYTD> as first into (/IndividualSurvey)[1]"#, &[]).unwrap();
        assert_eq!(d.to_xml(), r#"<IndividualSurvey xmlns="urn:s"><TotalPurchaseYTD>0.00</TotalPurchaseYTD><Year>1999</Year><Total>10.5</Total></IndividualSurvey>"#);
        d.modify("insert <Last/> as last into (/IndividualSurvey)[1]", &[]).unwrap();
        d.modify("insert <Mid/> after (/IndividualSurvey/Year)[1]", &[]).unwrap();
        d.modify("insert <Pre/> before (/IndividualSurvey/Year)[1]", &[]).unwrap();
        assert!(d.to_xml().contains("<Pre/><Year>1999</Year><Mid/><Total>10.5</Total><Last/>"), "{}", d.to_xml());
        d.modify("delete /IndividualSurvey/Mid", &[]).unwrap();
        d.modify("delete //Pre", &[]).unwrap();
        d.modify("replace value of (/IndividualSurvey/Total)[1] with data(/IndividualSurvey/Total)[1] + sql:arg(0)", &[Arg::Num(2.25)]).unwrap();
        d.modify("replace value of (/IndividualSurvey/Year)[1] with sql:arg(1)", &[Arg::Null, Arg::Str("2001".into())]).unwrap();
        d.modify(r#"insert attribute src {"web"} into (/IndividualSurvey)[1]"#, &[]).unwrap();
        d.modify(r#"insert <Note>{ sql:arg(0) } &amp; more</Note> into (/IndividualSurvey)[1]"#, &[Arg::Str("hi".into())]).unwrap();
        d.modify("replace value of (/IndividualSurvey/@src)[1] with 'x'", &[]).unwrap();
        assert_eq!(
            d.to_xml(),
            r#"<IndividualSurvey xmlns="urn:s" src="x"><TotalPurchaseYTD>0.00</TotalPurchaseYTD><Year>2001</Year><Total>12.75</Total><Last/><Note>hi &amp; more</Note></IndividualSurvey>"#
        );
        d.modify("delete /IndividualSurvey/@src", &[]).unwrap();
        assert!(!d.to_xml().contains("src="));
        // what cannot be done is an error, and changes nothing
        assert!(d.modify("insert <a/> into /IndividualSurvey/Nope", &[]).is_err());
        assert!(d.modify("insert <a/> into /IndividualSurvey/*", &[]).is_err(), "the target must be a single node");
        assert!(d.modify("replace value of /IndividualSurvey/* with 1", &[]).is_err());
        assert!(d.modify("rename node /a as b", &[]).is_err());
    }

    #[test]
    fn what_is_not_supported_is_an_error() {
        let d = doc("<a><b/></a>");
        assert!(d.select("for $x in /a return $x", &[]).is_err());
        assert!(d.select("/a/b[", &[]).is_err());
        assert!(d.select("unknown(1)", &[]).is_err());
    }
}
