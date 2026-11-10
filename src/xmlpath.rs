//! A small XML reader and XPath subset behind the `xml` methods `.value()`, `.exist()` and `.query()`.
//!
//! Supported path syntax: absolute or relative steps separated by `/` or `//`, element names, `*`, `@attribute`,
//! `text()`, `node()`-free predicates `[n]`, `[@a='v']`, `[name='v']`, `[last()]`, an optional wrapping
//! `( ... )[n]`, and `data( ... )`. Namespaces are ignored (prefixes are dropped from names). This is not XQuery:
//! FLWOR, functions other than `text()`, `data()` and `last()`, `.nodes()` and `.modify()` are not supported.
use std::fmt::Write as _;

#[derive(Clone, Debug)]
pub enum Node {
    Elem { name: String, attrs: Vec<(String, String)>, kids: Vec<Node> },
    Text(String),
}

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

/// Parse an XML fragment into its top-level nodes (a document has one element). `None` if it is not well-formed.
pub fn parse(src: &str) -> Option<Vec<Node>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut stack: Vec<(String, Vec<(String, String)>, Vec<Node>)> = vec![];
    let mut top: Vec<Node> = vec![];
    fn push(stack: &mut Vec<(String, Vec<(String, String)>, Vec<Node>)>, top: &mut Vec<Node>, n: Node) {
        match stack.last_mut() {
            Some(f) => f.2.push(n),
            None => top.push(n),
        }
    }
    while i < b.len() {
        if b[i] != b'<' {
            let end = src[i..].find('<').map(|k| i + k).unwrap_or(b.len());
            let t = &src[i..end];
            if !t.trim().is_empty() || !stack.is_empty() {
                push(&mut stack, &mut top, Node::Text(unescape(t)));
            }
            i = end;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->")? + 3;
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>")?;
            push(&mut stack, &mut top, Node::Text(rest[9..end].to_string()));
            i += end + 3;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            i += rest.find('>')? + 1;
        } else if rest.starts_with("</") {
            let end = rest.find('>')?;
            let name = rest[2..end].trim();
            let (n, attrs, kids) = stack.pop()?;
            if n != name {
                return None;
            }
            push(&mut stack, &mut top, Node::Elem { name: local(&n).to_string(), attrs, kids });
            i += end + 1;
        } else {
            // a start tag: name, attributes, optional '/'
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
                let value = unescape(&v[1..close]);
                if !an.starts_with("xmlns") {
                    attrs.push((local(&an).to_string(), value));
                }
                a = v[close + 1..].trim_start();
            }
            if empty {
                push(&mut stack, &mut top, Node::Elem { name: local(&name).to_string(), attrs, kids: vec![] });
            } else {
                stack.push((name, attrs, vec![]));
            }
            i = j + 1;
        }
    }
    if stack.is_empty() {
        Some(top)
    } else {
        None
    }
}

pub fn text_of(n: &Node) -> String {
    match n {
        Node::Text(t) => t.clone(),
        Node::Elem { kids, .. } => kids.iter().map(text_of).collect(),
    }
}

fn esc(s: &str, attr: bool) -> String {
    let mut o = String::new();
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

pub fn serialize(n: &Node) -> String {
    match n {
        Node::Text(t) => esc(t, false),
        Node::Elem { name, attrs, kids } => {
            let mut o = format!("<{}", name);
            for (k, v) in attrs {
                let _ = write!(o, " {}=\"{}\"", k, esc(v, true));
            }
            if kids.is_empty() {
                o.push_str("/>");
            } else {
                o.push('>');
                for k in kids {
                    o.push_str(&serialize(k));
                }
                let _ = write!(o, "</{}>", name);
            }
            o
        }
    }
}

/// What a path selects.
#[derive(Clone, Debug)]
pub enum Item {
    Node(Node),
    Attr(String),
}

impl Item {
    pub fn string(&self) -> String {
        match self {
            Item::Node(n) => text_of(n),
            Item::Attr(v) => v.clone(),
        }
    }
    pub fn xml(&self) -> String {
        match self {
            Item::Node(n) => serialize(n),
            Item::Attr(v) => esc(v, false),
        }
    }
}

#[derive(Debug)]
enum Pred {
    Index(usize),
    Last,
    Attr(String, String),
    Child(String, String),
}

#[derive(Debug)]
struct Step {
    descendant: bool,
    test: String, // element name, "*", "@name", "text()"
    preds: Vec<Pred>,
}

fn parse_path(path: &str) -> Option<(Vec<Step>, Option<Pred>)> {
    let mut p = path.trim();
    if let Some(inner) = p.strip_prefix("data(").and_then(|r| r.rsplit_once(')')) {
        // data(/a/b)[1]
        p = inner.0.trim();
        let tail = inner.1.trim();
        let whole = format!("({}){}", p, tail);
        return parse_path(&whole);
    }
    let mut outer = None;
    if p.starts_with('(') {
        let close = p.rfind(')')?;
        let tail = p[close + 1..].trim();
        if !tail.is_empty() {
            let n = tail.strip_prefix('[')?.strip_suffix(']')?.trim();
            outer = Some(if n == "last()" { Pred::Last } else { Pred::Index(n.parse().ok()?) });
        }
        p = p[1..close].trim();
    }
    let mut steps = vec![];
    let mut rest = p;
    if !rest.starts_with('/') {
        rest = rest.strip_prefix("./").unwrap_or(rest);
        // relative: treat as starting below the context
        let s = format!("/{}", rest);
        return parse_path_owned(&s, outer);
    }
    while !rest.is_empty() {
        let descendant = rest.starts_with("//");
        rest = if descendant { &rest[2..] } else { rest.strip_prefix('/')? };
        // step text up to the next '/' outside brackets
        let mut depth = 0;
        let mut end = rest.len();
        for (k, c) in rest.char_indices() {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                '/' if depth == 0 => {
                    end = k;
                    break;
                }
                _ => {}
            }
        }
        let (stext, tail) = rest.split_at(end);
        rest = tail;
        let (test, mut preds_text) = match stext.find('[') {
            Some(k) => (&stext[..k], &stext[k..]),
            None => (stext, ""),
        };
        let mut preds = vec![];
        while let Some(t) = preds_text.strip_prefix('[') {
            let close = t.find(']')?;
            let body = t[..close].trim();
            preds_text = &t[close + 1..];
            if body == "last()" {
                preds.push(Pred::Last);
            } else if let Ok(n) = body.parse::<usize>() {
                preds.push(Pred::Index(n));
            } else if let Some((l, r)) = body.split_once('=') {
                let v = r.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
                let l = l.trim();
                match l.strip_prefix('@') {
                    Some(a) => preds.push(Pred::Attr(local(a).to_string(), v)),
                    None => preds.push(Pred::Child(local(l).to_string(), v)),
                }
            } else {
                return None;
            }
        }
        steps.push(Step { descendant, test: local(test.trim()).to_string(), preds });
    }
    Some((steps, outer))
}

fn parse_path_owned(s: &str, outer: Option<Pred>) -> Option<(Vec<Step>, Option<Pred>)> {
    let (steps, o) = parse_path(s)?;
    Some((steps, outer.or(o)))
}

fn apply_preds(items: Vec<Node>, preds: &[Pred]) -> Vec<Node> {
    let mut cur = items;
    for p in preds {
        cur = match p {
            Pred::Index(n) => cur.into_iter().nth(n.saturating_sub(1)).into_iter().collect(),
            Pred::Last => cur.pop().into_iter().collect(),
            Pred::Attr(a, v) => cur.into_iter().filter(|n| matches!(n, Node::Elem { attrs, .. } if attrs.iter().any(|(k, x)| k == a && x == v))).collect(),
            Pred::Child(c, v) => cur
                .into_iter()
                .filter(|n| matches!(n, Node::Elem { kids, .. } if kids.iter().any(|k| matches!(k, Node::Elem { name, .. } if name == c) && text_of(k) == *v)))
                .collect(),
        };
    }
    cur
}

fn descendants(n: &Node, out: &mut Vec<Node>) {
    if let Node::Elem { kids, .. } = n {
        for k in kids {
            if matches!(k, Node::Elem { .. }) {
                out.push(k.clone());
            }
            descendants(k, out);
        }
    }
}

/// Evaluate a path against a document's top-level nodes. `Err` is a malformed or unsupported path.
pub fn select(doc: &[Node], path: &str) -> Result<Vec<Item>, String> {
    let (steps, outer) = parse_path(path).ok_or_else(|| format!("the path '{}' is not supported (only simple child, descendant and attribute steps)", path))?;
    // the document node is the context: its children are the top-level nodes
    let mut ctx: Vec<Node> = vec![Node::Elem { name: String::new(), attrs: vec![], kids: doc.to_vec() }];
    let mut items: Vec<Item> = vec![];
    for (si, st) in steps.iter().enumerate() {
        let last = si + 1 == steps.len();
        if let Some(attr) = st.test.strip_prefix('@') {
            if !last {
                return Err("an attribute step must be the last step".into());
            }
            for n in &ctx {
                let mut pool = vec![n.clone()];
                if st.descendant {
                    descendants(n, &mut pool);
                }
                for p in pool {
                    if let Node::Elem { attrs, .. } = &p {
                        for (k, v) in attrs {
                            if attr == "*" || k == attr {
                                items.push(Item::Attr(v.clone()));
                            }
                        }
                    }
                }
            }
            break;
        }
        if st.test == "text()" {
            if !last {
                return Err("text() must be the last step".into());
            }
            for n in &ctx {
                if let Node::Elem { kids, .. } = n {
                    for k in kids {
                        if let Node::Text(t) = k {
                            items.push(Item::Attr(t.clone()));
                        }
                    }
                }
            }
            break;
        }
        let mut next: Vec<Node> = vec![];
        for n in &ctx {
            let mut pool: Vec<Node> = vec![];
            if st.descendant {
                descendants(n, &mut pool);
            } else if let Node::Elem { kids, .. } = n {
                pool = kids.iter().filter(|k| matches!(k, Node::Elem { .. })).cloned().collect();
            }
            let matched: Vec<Node> = pool.into_iter().filter(|k| matches!(k, Node::Elem { name, .. } if st.test == "*" || *name == st.test)).collect();
            next.extend(apply_preds(matched, &st.preds));
        }
        ctx = next;
        if last {
            items = ctx.iter().cloned().map(Item::Node).collect();
        }
    }
    if let Some(p) = outer {
        items = match p {
            Pred::Index(n) => items.into_iter().nth(n.saturating_sub(1)).into_iter().collect(),
            Pred::Last => items.pop().into_iter().collect(),
            _ => items,
        };
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let d = parse(r#"<?xml version="1.0"?><EVENT_INSTANCE><EventType>CREATE_TABLE</EventType><Obj id="7" kind="t"><Name>a&amp;b</Name></Obj><Obj id="8"><Name>c</Name></Obj></EVENT_INSTANCE>"#).unwrap();
        let v = |p: &str| select(&d, p).unwrap().first().map(|i| i.string());
        assert_eq!(v("(/EVENT_INSTANCE/EventType)[1]").as_deref(), Some("CREATE_TABLE"));
        assert_eq!(v("/EVENT_INSTANCE/Obj[2]/Name").as_deref(), Some("c"));
        assert_eq!(v("(/EVENT_INSTANCE/Obj/@id)[2]").as_deref(), Some("8"));
        assert_eq!(v("/EVENT_INSTANCE/Obj[@id='7']/Name/text()").as_deref(), Some("a&b"));
        assert_eq!(v("//Name").as_deref(), Some("a&b"));
        assert_eq!(v("/EVENT_INSTANCE/Nope"), None);
        assert_eq!(select(&d, "/EVENT_INSTANCE/Obj").unwrap().len(), 2);
        assert_eq!(select(&d, "/EVENT_INSTANCE/Obj[last()]").unwrap()[0].xml(), "<Obj id=\"8\"><Name>c</Name></Obj>");
        assert!(parse("<a><b></a>").is_none());
    }
}
