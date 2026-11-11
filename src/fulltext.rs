//! Full-text search: the predicates `CONTAINS` and `FREETEXT` and the table functions `CONTAINSTABLE` and `FREETEXTTABLE`.
//!
//! SQL Server answers these from an inverted index that a full-text index maintains. This server has no such structure: a
//! text is tokenised and matched when a row is looked at, so a full-text search is a scan, and a full-text index is only
//! recorded in the catalog. What the matching understands:
//!
//! * words (case does not matter), `"phrases"`, prefix terms (`"comp*"`);
//! * `AND`, `OR`, `AND NOT` (also `&`, `|`, `&!`) and parentheses; `NEAR` (`a NEAR b`, `a ~ b` and `NEAR((a, b, c), distance,
//!   TRUE)`);
//! * `FORMSOF(INFLECTIONAL, word, ...)`: a word matches the texts that have a word with the same stem (a plain English
//!   suffix stripper; irregular forms are not known). `FORMSOF(THESAURUS, ...)` has no thesaurus to use and matches the word
//!   itself;
//! * `ISABOUT(term WEIGHT(w), ...)`: matches when any term matches;
//! * noise words (a short English list) are not searched for and a phrase skips over them, as in SQL Server.
//!
//! The rank of a match is a number from 1 to 1000 that grows with the number of occurrences found and the share of the terms
//! that matched. It orders results sensibly; it is not SQL Server's rank, and the same text does not get the same number.

/// Words that are ignored (SQL Server's English noise list is longer).
const NOISE: &[&str] = &[
    "a", "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "because", "been", "before", "being", "between", "both", "but", "by", "can", "could", "did", "do", "does", "each", "for", "from",
    "had", "has", "have", "he", "her", "here", "him", "his", "how", "i", "if", "in", "into", "is", "it", "its", "just", "may", "me", "more", "most", "my", "no", "not", "of", "on", "one", "only", "or", "other", "our",
    "out", "over", "she", "so", "some", "such", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "too", "under", "up", "us", "very", "was", "we", "were", "what", "when", "where",
    "which", "while", "who", "will", "with", "would", "you", "your",
];

pub fn is_noise(w: &str) -> bool {
    NOISE.binary_search(&w).is_ok()
}

/// The words of a text, lower-cased, in order (noise words included: they take a position).
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.extend(c.to_lowercase());
        } else {
            // an apostrophe or a hyphen inside a word does not split it into pieces of one letter
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A plain English stem: enough to put `runs`, `running` and `run` together.
pub fn stem(w: &str) -> String {
    let mut s = w.to_lowercase();
    if s.len() <= 3 {
        return s;
    }
    let ends = |s: &str, t: &str| s.ends_with(t);
    if ends(&s, "sses") {
        s.truncate(s.len() - 2);
    } else if ends(&s, "ies") && s.len() > 4 {
        s.truncate(s.len() - 3);
        s.push('y');
    } else if ends(&s, "ss") {
    } else if ends(&s, "s") {
        s.pop();
    }
    for suf in ["ingly", "edly", "ing", "ed", "ly"] {
        if s.len() > suf.len() + 2 && ends(&s, suf) {
            s.truncate(s.len() - suf.len());
            // runn -> run, but ll, ss, zz stay (call, pass)
            let b = s.as_bytes();
            if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] && !matches!(b[b.len() - 1], b'l' | b's' | b'z') {
                s.pop();
            }
            break;
        }
    }
    if s.len() > 3 && ends(&s, "e") {
        s.pop();
    }
    if s.len() > 3 && ends(&s, "y") {
        s.pop();
        s.push('i');
    }
    s
}

#[derive(Debug, Clone)]
enum Term {
    /// a word, or the words of a phrase (`None` stands for a noise word, which matches any one word)
    Words(Vec<Option<String>>),
    Prefix(String),
    /// all forms of the words
    Forms(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum Cond {
    Term(Term),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    AndNot(Box<Cond>, Box<Cond>),
    Near(Vec<Cond>, Option<usize>),
}

struct P<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.s[self.i..].starts_with(|c: char| c.is_whitespace()) {
            self.i += self.s[self.i..].chars().next().unwrap().len_utf8();
        }
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
    fn word_ahead(&mut self, w: &str) -> bool {
        self.ws();
        let r = &self.s[self.i..];
        r.len() >= w.len() && r[..w.len()].eq_ignore_ascii_case(w) && !r[w.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.word_ahead(w) {
            self.i += w.len();
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<Cond, String> {
        let mut l = self.and()?;
        loop {
            if self.eat_word("OR") || self.eat("|") {
                let r = self.and()?;
                l = Cond::Or(Box::new(l), Box::new(r));
            } else {
                return Ok(l);
            }
        }
    }

    fn and(&mut self) -> Result<Cond, String> {
        let mut l = self.near()?;
        loop {
            if self.eat_word("AND") {
                if self.eat_word("NOT") {
                    let r = self.near()?;
                    l = Cond::AndNot(Box::new(l), Box::new(r));
                } else {
                    let r = self.near()?;
                    l = Cond::And(Box::new(l), Box::new(r));
                }
            } else if self.eat("&!") {
                let r = self.near()?;
                l = Cond::AndNot(Box::new(l), Box::new(r));
            } else if self.eat("&") {
                let r = self.near()?;
                l = Cond::And(Box::new(l), Box::new(r));
            } else {
                return Ok(l);
            }
        }
    }

    /// `a NEAR b` and `a ~ b`
    fn near(&mut self) -> Result<Cond, String> {
        let first = self.primary()?;
        let mut parts = vec![first];
        while self.eat_word("NEAR") || self.eat("~") {
            parts.push(self.primary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Cond::Near(parts, None) })
    }

    fn quoted(&mut self) -> Result<String, String> {
        // the text after an opening quote, up to the closing one
        let r = &self.s[self.i..];
        let end = r.find('"').ok_or("a quoted term is not closed")?;
        let t = r[..end].to_string();
        self.i += end + 1;
        Ok(t)
    }

    fn term_of(text: &str) -> Result<Term, String> {
        let t = text.trim();
        if let Some(p) = t.strip_suffix('*') {
            let toks = tokens(p);
            if toks.len() == 1 {
                return Ok(Term::Prefix(toks.into_iter().next().unwrap()));
            }
            return Err("a prefix term is one word followed by *".into());
        }
        let toks: Vec<Option<String>> = tokens(t).into_iter().map(|w| if is_noise(&w) { None } else { Some(w) }).collect();
        if toks.is_empty() {
            return Err("a clause of the query contained only ignored words".into());
        }
        if toks.iter().all(|t| t.is_none()) {
            return Err("a clause of the query contained only ignored words".into());
        }
        Ok(Term::Words(toks))
    }

    fn primary(&mut self) -> Result<Cond, String> {
        self.ws();
        if self.eat("(") {
            let c = self.or()?;
            if !self.eat(")") {
                return Err("a '(' is not closed".into());
            }
            return Ok(c);
        }
        if self.eat("\"") {
            let q = self.quoted()?;
            return Ok(Cond::Term(Self::term_of(&q)?));
        }
        if self.word_ahead("FORMSOF") {
            self.i += 7;
            if !self.eat("(") {
                return Err("FORMSOF needs '('".into());
            }
            let inflectional = self.word_ahead("INFLECTIONAL");
            if !(self.eat_word("INFLECTIONAL") || self.eat_word("THESAURUS")) {
                return Err("FORMSOF takes INFLECTIONAL or THESAURUS".into());
            }
            let mut words = vec![];
            while self.eat(",") {
                self.ws();
                let w = if self.eat("\"") { self.quoted()? } else { self.bare()? };
                words.extend(tokens(&w).into_iter().filter(|w| !is_noise(w)));
            }
            if !self.eat(")") {
                return Err("FORMSOF is not closed".into());
            }
            if words.is_empty() {
                return Err("a clause of the query contained only ignored words".into());
            }
            // THESAURUS: no thesaurus, the words themselves
            return Ok(Cond::Term(if inflectional { Term::Forms(words) } else { Term::Words(words.into_iter().map(Some).collect()) }));
        }
        if self.word_ahead("ISABOUT") {
            self.i += 7;
            if !self.eat("(") {
                return Err("ISABOUT needs '('".into());
            }
            let mut c: Option<Cond> = None;
            loop {
                let t = self.near()?;
                // WEIGHT(w) is read and does not change the matching
                if self.eat_word("WEIGHT") {
                    if !self.eat("(") {
                        return Err("WEIGHT needs '('".into());
                    }
                    while !self.s[self.i..].starts_with(')') && self.i < self.s.len() {
                        self.i += 1;
                    }
                    self.eat(")");
                }
                c = Some(match c {
                    Some(p) => Cond::Or(Box::new(p), Box::new(t)),
                    None => t,
                });
                if !self.eat(",") {
                    break;
                }
            }
            if !self.eat(")") {
                return Err("ISABOUT is not closed".into());
            }
            return c.ok_or_else(|| "ISABOUT needs a term".to_string());
        }
        if self.word_ahead("NEAR") && self.s[self.i + 4..].trim_start().starts_with('(') {
            self.i += 4;
            self.eat("(");
            // NEAR((a, b, c), distance [, order])
            let mut parts = vec![];
            if self.eat("(") {
                loop {
                    parts.push(self.near()?);
                    if !self.eat(",") {
                        break;
                    }
                }
                if !self.eat(")") {
                    return Err("the terms of NEAR are not closed".into());
                }
            } else {
                parts.push(self.near()?);
                while self.eat(",") {
                    parts.push(self.near()?);
                }
            }
            let mut dist = None;
            if self.eat(",") {
                self.ws();
                let r = &self.s[self.i..];
                let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
                if end == 0 {
                    // MAX
                    if self.eat_word("MAX") {}
                } else {
                    dist = r[..end].parse::<usize>().ok();
                    self.i += end;
                }
                if self.eat(",") {
                    // match order: TRUE or FALSE; read, not used
                    self.eat_word("TRUE");
                    self.eat_word("FALSE");
                }
            }
            if !self.eat(")") {
                return Err("NEAR is not closed".into());
            }
            return Ok(Cond::Near(parts, dist));
        }
        let w = self.bare()?;
        Ok(Cond::Term(Self::term_of(&w)?))
    }

    fn bare(&mut self) -> Result<String, String> {
        self.ws();
        let r = &self.s[self.i..];
        let end = r.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '\'' || c == '*')).unwrap_or(r.len());
        if end == 0 {
            return Err(format!("a search term is expected at '{}'", r.chars().take(12).collect::<String>()));
        }
        self.i += end;
        Ok(r[..end].to_string())
    }
}

/// Parse the search condition of `CONTAINS`.
pub fn parse_contains(q: &str) -> Result<Cond, String> {
    let mut p = P { s: q, i: 0 };
    let c = p.or()?;
    p.ws();
    if p.i < q.len() {
        return Err(format!("syntax error in the search condition near '{}'", q[p.i..].chars().take(12).collect::<String>()));
    }
    Ok(c)
}

/// Positions at which a term matches in a token list.
fn hits(t: &Term, toks: &[String]) -> Vec<usize> {
    match t {
        Term::Prefix(p) => toks.iter().enumerate().filter(|(_, w)| w.starts_with(p.as_str())).map(|(i, _)| i).collect(),
        Term::Forms(ws) => {
            let stems: Vec<String> = ws.iter().map(|w| stem(w)).collect();
            toks.iter().enumerate().filter(|(_, w)| stems.contains(&stem(w))).map(|(i, _)| i).collect()
        }
        Term::Words(ws) => {
            let mut out = vec![];
            if toks.len() < ws.len() {
                return out;
            }
            'start: for i in 0..=toks.len() - ws.len() {
                for (k, w) in ws.iter().enumerate() {
                    if let Some(w) = w {
                        if &toks[i + k] != w {
                            continue 'start;
                        }
                    }
                }
                out.push(i);
            }
            out
        }
    }
}

/// Positions at which a condition matches (the position of the first word); empty when it does not.
fn eval(c: &Cond, toks: &[String]) -> Vec<usize> {
    match c {
        Cond::Term(t) => hits(t, toks),
        Cond::And(a, b) => {
            let (x, y) = (eval(a, toks), eval(b, toks));
            if x.is_empty() || y.is_empty() {
                vec![]
            } else {
                let mut v = x;
                v.extend(y);
                v.sort_unstable();
                v
            }
        }
        Cond::Or(a, b) => {
            let mut v = eval(a, toks);
            v.extend(eval(b, toks));
            v.sort_unstable();
            v.dedup();
            v
        }
        Cond::AndNot(a, b) => {
            if eval(b, toks).is_empty() {
                eval(a, toks)
            } else {
                vec![]
            }
        }
        Cond::Near(parts, dist) => {
            let lists: Vec<Vec<usize>> = parts.iter().map(|p| eval(p, toks)).collect();
            if lists.iter().any(|l| l.is_empty()) {
                return vec![];
            }
            // some choice of one position per term, all within `dist` words of each other (the span, less the words)
            let mut out = vec![];
            for &p0 in &lists[0] {
                let (mut lo, mut hi) = (p0, p0);
                let mut ok = true;
                for l in &lists[1..] {
                    // the nearest position to p0
                    let best = l.iter().min_by_key(|&&p| (p as i64 - p0 as i64).abs()).copied().unwrap();
                    lo = lo.min(best);
                    hi = hi.max(best);
                    ok &= true;
                }
                let between = hi - lo + 1 - parts.len();
                if ok && dist.map(|d| between <= d).unwrap_or(true) {
                    out.push(p0);
                }
            }
            out
        }
    }
}

fn rank_of(matches: usize, texts_len: usize) -> u32 {
    let _ = texts_len;
    (50 + 70 * matches as u32).min(1000)
}

/// The rank of `CONTAINS` of one or more texts (the columns): 0 when none matches.
pub fn contains_rank(c: &Cond, texts: &[&str]) -> u32 {
    let mut best = 0;
    for t in texts {
        let toks = tokens(t);
        let m = eval(c, &toks).len();
        if m > 0 {
            best = best.max(rank_of(m, toks.len()));
        }
    }
    best
}

/// The rank of `FREETEXT`: the words of the query (noise words left out) are matched by stem, and any of them matches.
pub fn freetext_rank(query: &str, texts: &[&str]) -> u32 {
    let words: Vec<String> = tokens(query).into_iter().filter(|w| !is_noise(w)).collect();
    if words.is_empty() {
        return 0;
    }
    let stems: Vec<String> = words.iter().map(|w| stem(w)).collect();
    let mut best = 0;
    for t in texts {
        let toks: Vec<String> = tokens(t).iter().map(|w| stem(w)).collect();
        let mut matched = 0;
        let mut occurrences = 0;
        for s in &stems {
            let n = toks.iter().filter(|w| *w == s).count();
            if n > 0 {
                matched += 1;
                occurrences += n;
            }
        }
        if matched > 0 {
            let r = (40 + 400 * matched as u32 / stems.len() as u32 + 40 * occurrences as u32).min(1000);
            best = best.max(r);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(q: &str, text: &str) -> bool {
        contains_rank(&parse_contains(q).unwrap(), &[text]) > 0
    }

    #[test]
    fn noise_words_are_sorted() {
        let mut v = NOISE.to_vec();
        v.sort_unstable();
        assert_eq!(v, NOISE.to_vec(), "binary search needs the list in order");
    }

    #[test]
    fn terms_phrases_and_prefixes() {
        let t = "The quick brown fox jumps over the lazy dog. Computing and computers are fun.";
        assert!(c("fox", t) && c("FOX", t) && !c("cat", t));
        assert!(c("\"brown fox\"", t) && !c("\"fox brown\"", t));
        assert!(c("\"quick brown fox jumps\"", t));
        // noise words in a phrase are skipped over
        assert!(c("\"jumps over the lazy\"", t) && c("\"fox the dog\"", t) == false);
        assert!(c("\"comput*\"", t) && !c("\"xyz*\"", t));
        assert!(c("fox AND dog", t) && !c("fox AND cat", t));
        assert!(c("fox OR cat", t) && !c("cat OR bird", t));
        assert!(c("fox AND NOT cat", t) && !c("fox AND NOT dog", t));
        assert!(c("fox & dog", t) && c("fox | cat", t) && c("fox &! cat", t));
        assert!(c("(cat OR dog) AND fox", t) && !c("(cat OR bird) AND fox", t));
    }

    #[test]
    fn proximity_and_forms() {
        let t = "alpha one two three beta";
        assert!(c("alpha NEAR beta", t) && c("alpha ~ beta", t));
        assert!(c("NEAR((alpha, beta), 3)", t) && !c("NEAR((alpha, beta), 2)", t));
        assert!(c("NEAR((alpha, beta), 5, TRUE)", t));
        assert!(c("FORMSOF(INFLECTIONAL, run)", "he is running home") && c("FORMSOF(INFLECTIONAL, runs)", "they run") && !c("FORMSOF(INFLECTIONAL, walk)", "they run"));
        assert!(c("FORMSOF(INFLECTIONAL, \"manager\")", "managers meet") && c("FORMSOF(INFLECTIONAL, call)", "he called"));
        assert!(c("FORMSOF(THESAURUS, run)", "they run") && !c("FORMSOF(THESAURUS, run)", "they ran"));
        assert!(c("ISABOUT(fox WEIGHT(.8), cat WEIGHT(.2))", t.replace("alpha", "fox").as_str()));
    }

    #[test]
    fn errors_and_rank() {
        assert!(parse_contains("the").is_err(), "only noise words");
        assert!(parse_contains("fox AND").is_err());
        assert!(parse_contains("(fox").is_err());
        assert!(parse_contains("\"fox").is_err());
        let r1 = contains_rank(&parse_contains("fox").unwrap(), &["a fox"]);
        let r3 = contains_rank(&parse_contains("fox").unwrap(), &["fox fox fox"]);
        assert!(r1 > 0 && r3 > r1 && r3 <= 1000);
        assert_eq!(contains_rank(&parse_contains("fox").unwrap(), &["cat"]), 0);
        // several columns: the best one counts
        assert!(contains_rank(&parse_contains("fox").unwrap(), &["cat", "fox"]) > 0);
        assert!(freetext_rank("running foxes", &["the fox ran"]) > 0, "foxes and fox have one stem");
        assert_eq!(freetext_rank("the of", &["the fox"]), 0, "only noise words");
        assert!(freetext_rank("quick brown fox", &["quick brown fox"]) > freetext_rank("quick brown fox", &["a fox"]));
    }

    #[test]
    fn stems() {
        for (a, b) in [("running", "run"), ("runs", "run"), ("managers", "manager"), ("called", "call"), ("studies", "study"), ("boxes", "box"), ("quickly", "quick")] {
            assert_eq!(stem(a), stem(b), "{} {}", a, b);
        }
        assert_ne!(stem("call"), stem("cell"));
    }
}
