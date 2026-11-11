//! `hyperstorctl --help` and `hyperstorctl help [COMMAND]`.
//!
//! The text is read from the command reference page
//! (`docs/public/src/reference/hyperstorctl.md`), the tables the
//! documentation is built from, so a command or an option is described
//! once, and `scripts/cli-docs-check.py` keeps the page and the binary in
//! step. Only what the page's two tables say is printed here: a command's
//! description, and the options whose description names it.

use std::fmt::Write;

const REFERENCE: &str = include_str!("../../../docs/public/src/reference/hyperstorctl.md");

/// Options every command takes: how to reach the cluster and how to print
/// the answer. Listed once in the overview, not under each command.
const GLOBAL: &[&str] = &[
    "--api",
    "--tls",
    "--socket",
    "--mgmt",
    "--token-file",
    "--mgmt-ca",
    "--in-realm",
    "--timeout-secs",
    "--json",
];

/// Where lines are wrapped.
const WIDTH: usize = 78;

/// One row of a reference table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The command (`snapshot list`), or the option as documented
    /// (`--size S`).
    pub name: String,
    /// An option's default; empty for a command.
    pub default: String,
    /// What it does, as the page writes it (Markdown).
    pub text: String,
}

/// The page's command and option tables.
#[derive(Debug, Default)]
pub struct Reference {
    pub commands: Vec<Row>,
    pub options: Vec<Row>,
}

/// What `handle` found to print.
#[derive(Debug, PartialEq, Eq)]
pub enum Help {
    /// Help text, for standard output, exit status 0.
    Text(String),
    /// A request for help that names nothing known: a message for standard
    /// error, exit status 2.
    Unknown(String),
}

/// A table row's cells. A bar written `\|` stays in its cell, without the
/// backslash.
fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for c in line.chars() {
        if c == '|' && escaped {
            cell.pop();
            cell.push('|');
        } else if c == '|' {
            out.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(c);
        }
        escaped = c == '\\';
    }
    out.push(cell.trim().to_string());
    out
}

/// Markdown as plain text: links keep their words, code loses its quotes.
pub fn plain(md: &str) -> String {
    let mut out = String::new();
    let mut rest = md;
    while let Some(i) = rest.find('[') {
        let after = &rest[i + 1..];
        if let Some(j) = after.find(']')
            && after[j + 1..].starts_with('(')
            && let Some(k) = after[j + 2..].find(')')
        {
            out.push_str(&rest[..i]);
            out.push_str(&after[..j]);
            rest = &after[j + 2 + k + 1..];
        } else {
            out.push_str(&rest[..=i]);
            rest = after;
        }
    }
    out.push_str(rest);
    out.replace('`', "")
}

/// The command and option tables of the reference page.
pub fn parse(page: &str) -> Reference {
    #[derive(PartialEq)]
    enum In {
        Nothing,
        Commands,
        Options,
    }
    let mut state = In::Nothing;
    let mut r = Reference::default();
    for line in page.lines() {
        if !line.starts_with('|') {
            state = In::Nothing;
            continue;
        }
        let c = cells(line);
        let first = c.first().map(String::as_str).unwrap_or("");
        if first == "Command" {
            state = In::Commands;
            continue;
        }
        if first == "Option" {
            state = In::Options;
            continue;
        }
        if !first.starts_with('`') {
            // The rule under a header keeps the table; another header ends it.
            if !first.starts_with('-') {
                state = In::Nothing;
            }
            continue;
        }
        let name = first.trim_matches('`').to_string();
        match state {
            In::Commands if c.len() >= 2 => r.commands.push(Row {
                name,
                default: String::new(),
                text: c[1].clone(),
            }),
            In::Options if c.len() >= 3 => r.options.push(Row {
                name,
                default: plain(&c[1]),
                text: c[2].clone(),
            }),
            _ => {}
        }
    }
    r
}

/// `text` in lines of at most `WIDTH` characters, each indented.
fn wrap(text: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && indent + line.chars().count() + 1 + word.chars().count() > WIDTH {
            let _ = writeln!(out, "{pad}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        let _ = writeln!(out, "{pad}{line}");
    }
    out
}

/// The first sentence of a description, in plain words, cut to `max`
/// characters at a word.
fn summary(md: &str, max: usize) -> String {
    let t = plain(md);
    let end = t.find(". ").unwrap_or(t.len());
    let s = t[..end].trim_end_matches('.');
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(3)).collect();
    let head = match head.rsplit_once(' ') {
        Some((words, _)) => words,
        None => head.as_str(),
    };
    format!("{}...", head.trim_end_matches([',', ';', ':', ' ']))
}

/// An option's name without its argument: `--size S` is `--size`.
fn key(option: &str) -> &str {
    option.split_whitespace().next().unwrap_or(option)
}

/// The `--options` a text names.
fn option_names(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'-' && b[i + 1] == b'-' && b[i + 2].is_ascii_lowercase() {
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_lowercase() || b[j] == b'-') {
                j += 1;
            }
            out.push(text[i..j].trim_end_matches('-').to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// An option's description cut into the sentences that start with a
/// command's name in code: `a`: one thing. `b`: another.
fn pieces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(". `") {
        out.push(rest[..=i].to_string());
        rest = &rest[i + 2..];
    }
    out.push(rest.to_string());
    out
}

/// Whether a piece of an option's description names `cmd`, or its group
/// (`claim ...`).
fn mentions(piece: &str, cmd: &str, group: Option<&str>) -> bool {
    piece.contains(&format!("`{cmd}`")) || group.is_some_and(|g| piece.contains(g))
}

/// The sentences of an option's description that are about `cmd`, with
/// the general one before them if there is one; `None` if none names it.
fn relevant(text: &str, cmd: &str, group: Option<&str>) -> Option<String> {
    let ps = pieces(text);
    let hits: Vec<&String> = ps.iter().filter(|p| mentions(p, cmd, group)).collect();
    if hits.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    if !text.starts_with('`') && !mentions(&ps[0], cmd, group) {
        out.push(ps[0].clone());
    }
    out.extend(hits.into_iter().cloned());
    Some(out.join(" "))
}

/// Every command, one line each, then the options all of them take.
fn overview(r: &Reference) -> String {
    let w = r
        .commands
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut s = String::new();
    s.push_str("hyperstorctl: manage and test a Hyperstor cluster through the cluster API.\n\n");
    s.push_str("Usage: hyperstorctl [CONNECTION OPTIONS] COMMAND [OPTIONS]\n\n");
    s.push_str("Commands (`hyperstorctl help COMMAND` describes one, with its options):\n");
    for c in &r.commands {
        let room = WIDTH.saturating_sub(4 + w);
        let _ = writeln!(s, "  {:<w$}  {}", c.name, summary(&c.text, room));
    }
    s.push_str("\nConnection and output options, for every command:\n");
    let globals: Vec<&Row> = GLOBAL
        .iter()
        .filter_map(|g| r.options.iter().find(|o| key(&o.name) == *g))
        .collect();
    let gw = globals
        .iter()
        .map(|o| o.name.chars().count())
        .max()
        .unwrap_or(0);
    for o in globals {
        let room = WIDTH.saturating_sub(4 + gw);
        let _ = writeln!(s, "  {:<gw$}  {}", o.name, summary(&o.text, room));
    }
    s.push_str(
        "\nThe full reference is docs/public/src/reference/hyperstorctl.md.\n\
         Exit status: 0 done, 1 failed, 2 a missing or malformed option,\n\
         3 the cluster API can't be reached, 4 not allowed.\n",
    );
    s
}

/// One command: what it does and the options that go with it.
fn command(r: &Reference, name: &str) -> Option<String> {
    let c = r.commands.iter().find(|c| c.name == name)?;
    let mut s = format!("hyperstorctl {}\n\n", c.name);
    s.push_str(&wrap(&plain(&c.text), 2));
    let named = option_names(&c.text);
    let group = c.name.split_once(' ').map(|(g, _)| format!("`{g} ...`"));
    let mut opts = String::new();
    for o in &r.options {
        let k = key(&o.name);
        if GLOBAL.contains(&k) {
            continue;
        }
        let about = relevant(&o.text, &c.name, group.as_deref());
        if about.is_none() && !named.iter().any(|n| n == k) {
            continue;
        }
        // An option the command's own description names, but whose row is
        // about other commands (`--file`, `--id`), is left out: the
        // description already says what it is for here. A row that starts
        // with a general sentence gives that sentence.
        let text = match about {
            Some(t) => t,
            None if o.text.starts_with('`') => continue,
            None => pieces(&o.text).swap_remove(0),
        };
        let _ = writeln!(opts, "  {}", o.name);
        opts.push_str(&wrap(&plain(&text), 6));
        if !o.default.is_empty() {
            let _ = writeln!(opts, "      Default: {}", o.default);
        }
    }
    if !opts.is_empty() {
        s.push_str("\nOptions:\n");
        s.push_str(&opts);
    }
    s.push_str(
        "\nEvery command also takes the connection options and --json;\n\
         `hyperstorctl --help` lists them.\n",
    );
    Some(s)
}

/// The commands of a group (`snapshot`), one line each.
fn group(r: &Reference, word: &str) -> Option<String> {
    let prefix = format!("{word} ");
    let members: Vec<&Row> = r
        .commands
        .iter()
        .filter(|c| c.name.starts_with(&prefix))
        .collect();
    if members.is_empty() {
        return None;
    }
    let w = members
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut s = format!("hyperstorctl {word} COMMAND\n\n");
    for c in members {
        let room = WIDTH.saturating_sub(4 + w);
        let _ = writeln!(s, "  {:<w$}  {}", c.name, summary(&c.text, room));
    }
    s.push_str("\n`hyperstorctl help COMMAND` describes one, with its options.\n");
    Some(s)
}

/// The command, or group of commands, the words name: two words first
/// (`snapshot list`), then one; words that are an option's value (a pool
/// number) name nothing and are passed over.
fn lookup(r: &Reference, words: &[&str]) -> Option<String> {
    for i in 0..words.len() {
        if i + 1 < words.len()
            && let Some(t) = command(r, &format!("{} {}", words[i], words[i + 1]))
        {
            return Some(t);
        }
        if let Some(t) = command(r, words[i]).or_else(|| group(r, words[i])) {
            return Some(t);
        }
    }
    None
}

/// Whether the arguments ask for help: `help [COMMAND]` first, or `--help`
/// or `-h` anywhere (`create-volume --help`). `None` when they don't, for
/// the command to run.
pub fn handle(argv: &[String]) -> Option<Help> {
    let (words, spelled_help): (Vec<&str>, bool) =
        if argv.first().map(String::as_str) == Some("help") {
            (
                argv[1..]
                    .iter()
                    .map(String::as_str)
                    .filter(|a| !a.starts_with('-'))
                    .collect(),
                true,
            )
        } else if argv.iter().any(|a| a == "--help" || a == "-h") {
            (
                argv.iter()
                    .map(String::as_str)
                    .filter(|a| !a.starts_with('-'))
                    .collect(),
                false,
            )
        } else {
            return None;
        };
    let r = parse(REFERENCE);
    if words.is_empty() {
        return Some(Help::Text(overview(&r)));
    }
    match lookup(&r, &words) {
        Some(t) => Some(Help::Text(t)),
        None if spelled_help => Some(Help::Unknown(format!(
            "hyperstorctl: no command \"{}\"; `hyperstorctl --help` lists them",
            words.join(" ")
        ))),
        None => Some(Help::Text(overview(&r))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cells_keep_an_escaped_bar_in_its_cell() {
        assert_eq!(
            cells("| `--heat read\\|write` | | `pg-grid`: a | b |"),
            vec!["`--heat read|write`", "", "`pg-grid`: a", "b"]
        );
    }

    #[test]
    fn plain_keeps_the_words_of_a_link_and_drops_code_quotes() {
        assert_eq!(
            plain("Go through [hyperstor-mgmt](hyperstor-mgmt.md) with `--mgmt`"),
            "Go through hyperstor-mgmt with --mgmt"
        );
        // Brackets that aren't a link stay as they are.
        assert_eq!(
            plain("`--map ADDR[,ADDR...]` and [x](y)"),
            "--map ADDR[,ADDR...] and x"
        );
    }

    #[test]
    fn the_page_gives_every_command_and_option() {
        let r = parse(REFERENCE);
        assert!(r.commands.len() > 80, "{} commands", r.commands.len());
        assert!(r.options.len() > 50, "{} options", r.options.len());
        for c in ["status", "create-volume", "snapshot list", "realm host"] {
            assert!(r.commands.iter().any(|x| x.name == c), "no {c}");
        }
        for o in ["--api", "--size", "--json"] {
            assert!(r.options.iter().any(|x| key(&x.name) == o), "no {o}");
        }
        assert!(r.commands.iter().all(|c| !c.text.is_empty()));
        assert!(r.options.iter().all(|o| !o.text.is_empty()));
    }

    #[test]
    fn a_command_shows_the_sentences_of_its_options_that_are_about_it() {
        let r = parse(REFERENCE);
        let t = command(&r, "create-volume").expect("a command");
        assert!(t.contains("--size S"), "{t}");
        assert!(t.contains("its id, instead of a random one"), "{t}");
        // `--id` also serves other commands; their sentences are left out.
        assert!(!t.contains("the token's id"), "{t}");
        assert!(!t.contains("--serial"), "{t}");
    }

    #[test]
    fn an_option_the_description_names_but_the_row_does_not_cover_is_left_out() {
        let r = parse(REFERENCE);
        let t = command(&r, "write-volume").expect("a command");
        // The description says what --file and --id are here; their rows
        // are about `cert show` and `delete-volume`.
        assert!(!t.contains("a certificate file"), "{t}");
        assert!(!t.contains("32 hex digits"), "{t}");
        assert!(t.contains("--offset O"), "{t}");
    }

    #[test]
    fn a_cut_summary_does_not_end_on_punctuation() {
        let s = summary(
            "Health (ok, warn or error) with its reasons, nodes and devices up",
            50,
        );
        assert!(s.ends_with("..."), "{s}");
        assert!(
            !s.trim_end_matches("...").ends_with([',', ';', ':', ' ']),
            "{s}"
        );
    }

    #[test]
    fn help_is_asked_for_by_word_or_flag() {
        assert_eq!(handle(&args(&["status"])), None);
        let Some(Help::Text(a)) = handle(&args(&["help", "snapshot", "list"])) else {
            panic!("no help")
        };
        let Some(Help::Text(b)) = handle(&args(&["--pool", "1", "snapshot", "list", "--help"]))
        else {
            panic!("no help")
        };
        assert_eq!(a, b);
        assert!(a.starts_with("hyperstorctl snapshot list\n"), "{a}");
        let Some(Help::Text(g)) = handle(&args(&["snapshot", "-h"])) else {
            panic!("no help")
        };
        assert!(g.contains("snapshot group-delete"), "{g}");
        assert!(
            matches!(handle(&args(&["help", "nonsense"])), Some(Help::Unknown(m)) if m.contains("nonsense"))
        );
        // With --help, words that name nothing give the overview.
        let Some(Help::Text(o)) = handle(&args(&["--api", "x:1", "--help"])) else {
            panic!("no help")
        };
        assert!(o.contains("Commands ("), "{o}");
    }

    #[test]
    fn no_line_is_longer_than_the_width() {
        let r = parse(REFERENCE);
        let mut all = vec![overview(&r)];
        all.extend(r.commands.iter().filter_map(|c| command(&r, &c.name)));
        for t in all {
            for l in t.lines() {
                assert!(
                    l.chars().count() <= WIDTH,
                    "{} chars: {l}",
                    l.chars().count()
                );
            }
        }
    }
}
