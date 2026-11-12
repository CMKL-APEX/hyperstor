//! S3's XML: replies written as elements and escaped text, and request
//! bodies (DeleteObjects', CompleteMultipartUpload's) read into a tree of
//! elements. The reader takes the XML that S3's clients send: elements,
//! text, entities, CDATA, comments and a prolog; attributes are skipped
//! and namespace prefixes dropped.

use std::borrow::Cow;

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

/// A document as read: its elements in document order, each with its
/// name without a namespace prefix, its text (entities decoded) and
/// where its subtree ends. Names and text borrow from the body where
/// they can, and the elements are one allocation, sized before reading
/// from the body's start tags: a tree costs 48 bytes an element beside
/// the body, and text is copied only when it has references or comes in
/// pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc<'a> {
    nodes: Vec<Node<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node<'a> {
    name: &'a str,
    text: Cow<'a, str>,
    /// The index past this element's last descendant.
    end: u32,
}

impl<'a> Doc<'a> {
    /// The root element.
    pub fn root(&self) -> Element<'_, 'a> {
        Element {
            nodes: &self.nodes,
            i: 0,
        }
    }
}

/// An element of a [`Doc`].
#[derive(Debug, Clone, Copy)]
pub struct Element<'d, 'a> {
    nodes: &'d [Node<'a>],
    i: usize,
}

impl<'d, 'a> Element<'d, 'a> {
    fn node(&self) -> &'d Node<'a> {
        &self.nodes[self.i]
    }

    pub fn name(&self) -> &'a str {
        self.node().name
    }

    pub fn text(&self) -> &'d str {
        &self.node().text
    }

    /// Its child elements, in order.
    pub fn children(self) -> impl Iterator<Item = Element<'d, 'a>> {
        let (nodes, end) = (self.nodes, self.node().end as usize);
        let mut at = self.i + 1;
        std::iter::from_fn(move || {
            (at < end).then(|| {
                let e = Element { nodes, i: at };
                at = nodes[at].end as usize;
                e
            })
        })
    }

    /// The first child named `name`.
    pub fn child(&self, name: &str) -> Option<Element<'d, 'a>> {
        self.children().find(|c| c.name() == name)
    }

    /// Every child named `name`.
    pub fn all(self, name: &str) -> impl Iterator<Item = Element<'d, 'a>> {
        self.children().filter(move |c| c.name() == name)
    }

    /// The text of the first child named `name`.
    pub fn text_of(&self, name: &str) -> Option<&'d str> {
        self.child(name).map(|c| c.text())
    }
}

/// Adds a piece of text to `t`. Once an element has children, the
/// whitespace between them is layout and dropped: S3's bodies don't mix
/// text and elements.
fn push_text<'a>(t: &mut Cow<'a, str>, piece: Cow<'a, str>, has_children: bool) {
    if piece.is_empty() || (has_children && blank(&piece)) {
        return;
    }
    if t.is_empty() {
        *t = piece;
    } else {
        t.to_mut().push_str(&piece);
    }
}

fn blank(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_whitespace())
}

/// Elements nested deeper than this are refused.
const MAX_DEPTH: usize = 16;

/// Reads `text`, a document of one root element and at most `max`
/// elements in all: the caller's bound on the tree.
pub fn parse(text: &str, max: usize) -> Result<Doc<'_>, &'static str> {
    // Start tags (a '<' before a name) bound the elements from above;
    // reserving for them once keeps the vector from growing by doubling.
    let tags = text
        .as_bytes()
        .windows(2)
        .filter(|w| w[0] == b'<' && !matches!(w[1], b'/' | b'!' | b'?'))
        .count();
    let mut r = Reader {
        s: text,
        at: 0,
        max,
        nodes: Vec::with_capacity(tags.min(max)),
    };
    r.misc()?;
    r.element(0)?;
    r.misc()?;
    if r.at != r.s.len() {
        return Err("text after the root element");
    }
    Ok(Doc { nodes: r.nodes })
}

struct Reader<'a> {
    s: &'a str,
    at: usize,
    /// The most elements allowed.
    max: usize,
    nodes: Vec<Node<'a>>,
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

    /// An element and its descendants, onto `nodes`.
    fn element(&mut self, depth: usize) -> Result<(), &'static str> {
        if depth > MAX_DEPTH {
            return Err("elements nested too deep");
        }
        if self.nodes.len() >= self.max {
            return Err("too many elements");
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
        let me = self.nodes.len();
        self.nodes.push(Node {
            name: qname.split_once(':').map_or(qname, |(_, n)| n),
            text: Cow::Borrowed(""),
            end: 0,
        });
        let close = |nodes: &mut Vec<Node<'a>>| {
            // Within u32: the caller's `max` is far below it.
            nodes[me].end = u32::try_from(nodes.len()).unwrap_or(u32::MAX);
        };
        if empty {
            close(&mut self.nodes);
            return Ok(());
        }
        let mut text = Cow::Borrowed("");
        let mut has_children = false;
        loop {
            let r = self.rest();
            if r.is_empty() {
                return Err("an element isn't closed");
            }
            if let Some(t) = r.strip_prefix("</") {
                let i = t.find('>').ok_or("unterminated tag")?;
                if t[..i].trim_end() != qname {
                    return Err("an element closed by another's tag");
                }
                self.at += 2 + i + 1;
                self.nodes[me].text = text;
                close(&mut self.nodes);
                return Ok(());
            } else if r.starts_with("<!--") {
                self.past("-->")?;
            } else if let Some(t) = r.strip_prefix("<![CDATA[") {
                let i = t.find("]]>").ok_or("unterminated CDATA")?;
                push_text(&mut text, Cow::Borrowed(&t[..i]), has_children);
                self.at += "<![CDATA[".len() + i + 3;
            } else if r.starts_with("<?") {
                self.past("?>")?;
            } else if r.starts_with('<') {
                if !has_children && blank(&text) {
                    text = Cow::Borrowed("");
                }
                has_children = true;
                self.element(depth + 1)?;
            } else {
                let i = r.find('<').unwrap_or(r.len());
                push_text(&mut text, unescape(&r[..i])?, has_children);
                self.at += i;
            }
        }
    }
}

/// `text` with its entity and character references decoded; borrowed
/// when it has none.
fn unescape(text: &str) -> Result<Cow<'_, str>, &'static str> {
    if !text.contains('&') {
        return Ok(Cow::Borrowed(text));
    }
    let mut out = String::with_capacity(text.len());
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
                code.and_then(char::from_u32)
                    .ok_or("an unknown reference")?
            }
        };
        out.push(c);
        rest = &t[end + 1..];
    }
    out.push_str(rest);
    Ok(Cow::Owned(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_read_as_written() {
        let doc = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- c -->\n<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Quiet>true</Quiet>\n  <Object><Key>a&lt;b&amp;&#x1;&#65;</Key></Object><Object><Key><![CDATA[<x>]]></Key><VersionId/></Object><s3:Object a='>'><Key>k</Key></s3:Object></Delete>";
        let d = parse(doc, 100).unwrap();
        let d = d.root();
        assert_eq!(d.name(), "Delete");
        assert_eq!(d.text_of("Quiet"), Some("true"));
        let keys: Vec<_> = d.all("Object").map(|o| o.text_of("Key").unwrap()).collect();
        assert_eq!(keys, ["a<b&\u{1}A", "<x>", "k"]);
        assert_eq!(
            d.all("Object").nth(1).unwrap().text_of("VersionId"),
            Some("")
        );
        // What a body written by Xml reads back as.
        let mut x = Xml::new("R");
        x.elem("K", "a<b>&\"'\u{1}\r");
        assert_eq!(
            parse(&x.finish("R"), 2).unwrap().root().text_of("K"),
            Some("a<b>&\"'\u{1}\r")
        );
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
            assert!(parse(bad, 100).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_elements_are_counted_and_text_borrowed() {
        let doc = "<a>\n  <b>x</b>\n  <b> <!-- c --> </b>\n <c><b/></c></a>";
        assert_eq!(parse(doc, 4), Err("too many elements"));
        let d = parse(doc, 5).unwrap();
        assert_eq!(d.nodes.capacity(), 5);
        let a = d.root();
        // Layout between children is dropped; a child's own text isn't.
        assert_eq!(a.text(), "");
        let names: Vec<_> = a.children().map(|e| e.name()).collect();
        assert_eq!(names, ["b", "b", "c"]);
        let texts: Vec<_> = a.all("b").map(|b| b.text()).collect();
        assert_eq!(texts, ["x", "  "]);
        assert_eq!(a.child("c").unwrap().children().count(), 1);
        assert!(matches!(d.nodes[1].text, Cow::Borrowed(_)));
        // Copied only when decoded or in pieces.
        let d = parse("<a>x&amp;y</a>", 1).unwrap();
        assert!(matches!(d.nodes[0].text, Cow::Owned(_)) && d.root().text() == "x&y");
        let d = parse("<a>x<![CDATA[<y>]]></a>", 1).unwrap();
        assert_eq!(d.root().text(), "x<y>");
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
