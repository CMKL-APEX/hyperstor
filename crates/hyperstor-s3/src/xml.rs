//! S3's XML: replies written as elements and escaped text, and request
//! bodies (DeleteObjects', CompleteMultipartUpload's) read into a tree of
//! elements. The reader takes the XML that S3's clients send: elements,
//! text, entities, CDATA, comments and a prolog; attributes are skipped
//! and namespace prefixes dropped.

pub const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

/// A document under construction.
pub struct Xml(String);

impl Xml {
    /// A document whose root is `root`, in S3's namespace.
    pub fn new(root: &str) -> Xml {
        Xml(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} xmlns=\"{NS}\">"
        ))
    }

    /// A document whose root is `root`, in no namespace: S3's errors.
    pub fn bare(root: &str) -> Xml {
        Xml(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root}>"
        ))
    }

    pub fn open(&mut self, name: &str) -> &mut Self {
        self.0.push_str(&format!("<{name}>"));
        self
    }

    pub fn close(&mut self, name: &str) -> &mut Self {
        self.0.push_str(&format!("</{name}>"));
        self
    }

    /// `<name>text</name>`, the text escaped.
    pub fn elem(&mut self, name: &str, text: &str) -> &mut Self {
        self.open(name);
        escape(&mut self.0, text);
        self.close(name)
    }

    pub fn finish(mut self, root: &str) -> String {
        self.close(root);
        self.0
    }
}

/// `text` escaped for XML. Control characters, which XML 1.0 can't hold
/// even escaped, are written `&#xN;` as S3 writes them; clients that
/// can't read them ask for `encoding-type=url`.
pub fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n') => {
                out.push_str(&format!("&#x{:X};", c as u32))
            }
            c => out.push(c),
        }
    }
}

/// An element as read: its name without a namespace prefix, its text
/// (entities decoded) and its child elements, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    /// The first child named `name`.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Every child named `name`.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The text of the first child named `name`.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

/// Elements nested deeper than this are refused.
const MAX_DEPTH: usize = 16;

/// Reads `text`, a document of one root element.
pub fn parse(text: &str) -> Result<Element, &'static str> {
    let mut r = Reader { s: text, at: 0 };
    r.misc()?;
    let root = r.element(0)?;
    r.misc()?;
    if r.at != r.s.len() {
        return Err("text after the root element");
    }
    Ok(root)
}

struct Reader<'a> {
    s: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.at..]
    }

    /// Skips past `end`, which must follow.
    fn past(&mut self, end: &str) -> Result<(), &'static str> {
        let i = self.rest().find(end).ok_or("unterminated markup")?;
        self.at += i + end.len();
        Ok(())
    }

    /// Whitespace, comments and processing instructions (the prolog).
    fn misc(&mut self) -> Result<(), &'static str> {
        loop {
            let t = self.rest().trim_start();
            self.at = self.s.len() - t.len();
            if t.starts_with("<?") {
                self.past("?>")?;
            } else if t.starts_with("<!--") {
                self.past("-->")?;
            } else if t.starts_with("<!DOCTYPE") {
                // Entities a DOCTYPE could declare aren't expanded.
                return Err("a DOCTYPE");
            } else {
                return Ok(());
            }
        }
    }

    fn element(&mut self, depth: usize) -> Result<Element, &'static str> {
        if depth > MAX_DEPTH {
            return Err("elements nested too deep");
        }
        if !self.rest().starts_with('<') {
            return Err("not an element");
        }
        self.at += 1;
        let end = self
            .rest()
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .ok_or("unterminated tag")?;
        let qname = &self.rest()[..end];
        if qname.is_empty() {
            return Err("a tag without a name");
        }
        let qname = qname.to_string();
        self.at += end;
        // Attributes, skipped; a quoted value may hold '>'.
        let mut empty = false;
        loop {
            let c = self.rest().chars().next().ok_or("unterminated tag")?;
            match c {
                '>' => {
                    self.at += 1;
                    break;
                }
                '/' if self.rest().starts_with("/>") => {
                    self.at += 2;
                    empty = true;
                    break;
                }
                '"' | '\'' => {
                    self.at += 1;
                    self.past(if c == '"' { "\"" } else { "'" })?;
                }
                c => self.at += c.len_utf8(),
            }
        }
        let name = match qname.split_once(':') {
            Some((_, n)) => n.to_string(),
            None => qname.clone(),
        };
        let mut e = Element {
            name,
            ..Default::default()
        };
        if empty {
            return Ok(e);
        }
        loop {
            let r = self.rest();
            if r.is_empty() {
                return Err("an element isn't closed");
            }
            if let Some(t) = r.strip_prefix("</") {
                let close = t.find('>').ok_or("unterminated tag")?;
                if t[..close].trim_end() != qname {
                    return Err("an element closed by another's tag");
                }
                self.at += 2 + close + 1;
                return Ok(e);
            } else if r.starts_with("<!--") {
                self.past("-->")?;
            } else if let Some(t) = r.strip_prefix("<![CDATA[") {
                let i = t.find("]]>").ok_or("unterminated CDATA")?;
                e.text.push_str(&t[..i]);
                self.at += "<![CDATA[".len() + i + 3;
            } else if r.starts_with("<?") {
                self.past("?>")?;
            } else if r.starts_with('<') {
                e.children.push(self.element(depth + 1)?);
            } else {
                let i = r.find('<').unwrap_or(r.len());
                unescape(&mut e.text, &r[..i])?;
                self.at += i;
            }
        }
    }
}

/// `text` with its entity and character references decoded, onto `out`.
fn unescape(out: &mut String, text: &str) -> Result<(), &'static str> {
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let t = &rest[i + 1..];
        let end = t.find(';').ok_or("an unterminated reference")?;
        let name = &t[..end];
        let c = match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            n => {
                let code = if let Some(h) = n.strip_prefix("#x").or_else(|| n.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = n.strip_prefix('#') {
                    d.parse().ok()
                } else {
                    None
                };
                code.and_then(char::from_u32).ok_or("an unknown reference")?
            }
        };
        out.push(c);
        rest = &t[end + 1..];
    }
    out.push_str(rest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_read_as_written() {
        let doc = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- c -->\n<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Quiet>true</Quiet>\n  <Object><Key>a&lt;b&amp;&#x1;&#65;</Key></Object><Object><Key><![CDATA[<x>]]></Key><VersionId/></Object><s3:Object a='>'><Key>k</Key></s3:Object></Delete>";
        let d = parse(doc).unwrap();
        assert_eq!(d.name, "Delete");
        assert_eq!(d.text_of("Quiet"), Some("true"));
        let keys: Vec<_> = d.all("Object").map(|o| o.text_of("Key").unwrap()).collect();
        assert_eq!(keys, ["a<b&\u{1}A", "<x>", "k"]);
        assert_eq!(d.all("Object").nth(1).unwrap().text_of("VersionId"), Some(""));
        // What a body written by Xml reads back as.
        let mut x = Xml::new("R");
        x.elem("K", "a<b>&\"'\u{1}\r");
        assert_eq!(parse(&x.finish("R")).unwrap().text_of("K"), Some("a<b>&\"'\u{1}\r"));
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a>&nope;</a>",
            "<a>&amp</a>",
            "<a/><b/>",
            "<!DOCTYPE a [<!ENTITY x \"y\">]><a>&x;</a>",
            &"<a>".repeat(20),
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn text_is_escaped() {
        let mut x = Xml::new("R");
        x.elem("K", "a<b>&\"'\u{1}\r");
        assert_eq!(
            x.finish("R"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<R xmlns=\"{NS}\"><K>a&lt;b&gt;&amp;&quot;&apos;&#x1;&#xD;</K></R>"
            )
        );
    }
}
