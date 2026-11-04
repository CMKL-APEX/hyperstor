//! S3's XML replies, written: elements and escaped text, nothing parsed.

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

#[cfg(test)]
mod tests {
    use super::*;

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
