//! `hyperstorctl`'s help and its messages for a wrong command line, run as
//! the binary: no cluster is needed, since none of these reaches the API.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hyperstorctl"))
        .args(args)
        .env_remove("HYPERSTOR_TIMEOUT")
        .output()
        .expect("run hyperstorctl")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn without_arguments_it_prints_one_usage_line_and_where_to_find_help() {
    let o = run(&[]);
    assert_eq!(o.status.code(), Some(2));
    let e = err(&o);
    // scripts/cli-docs-check.py needs exactly one such line, and the page
    // to quote it.
    assert_eq!(
        e.lines().filter(|l| l.starts_with("usage:")).count(),
        1,
        "{e}"
    );
    assert!(e.contains("hyperstorctl --help"), "{e}");
    assert!(out(&o).is_empty());
}

#[test]
fn help_lists_the_commands_and_exits_zero() {
    for args in [&["--help"][..], &["-h"], &["help"]] {
        let o = run(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", err(&o));
        let s = out(&o);
        for want in [
            "create-volume",
            "snapshot list",
            "realm host",
            "--api ADDR",
            "--json",
        ] {
            assert!(s.contains(want), "{args:?} lacks {want}:\n{s}");
        }
        assert!(err(&o).is_empty(), "{args:?}: {}", err(&o));
    }
}

#[test]
fn help_for_a_command_names_its_options() {
    let a = run(&["help", "create-volume"]);
    assert_eq!(a.status.code(), Some(0));
    let s = out(&a);
    assert!(s.starts_with("hyperstorctl create-volume\n"), "{s}");
    for want in [
        "--size S",
        "--pool P",
        "--name N",
        "its id, instead of a random one",
    ] {
        assert!(s.contains(want), "lacks {want}:\n{s}");
    }
    // The flag after the command says the same.
    let b = run(&["create-volume", "--help"]);
    assert_eq!(b.status.code(), Some(0));
    assert_eq!(out(&b), s);
}

#[test]
fn help_for_a_group_lists_its_commands() {
    let o = run(&["help", "snapshot"]);
    assert_eq!(o.status.code(), Some(0));
    let s = out(&o);
    for want in ["snapshot list", "snapshot create", "snapshot group-delete"] {
        assert!(s.contains(want), "lacks {want}:\n{s}");
    }
}

#[test]
fn help_for_nothing_known_is_refused_by_name() {
    let o = run(&["help", "make-coffee"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("make-coffee"), "{}", err(&o));
    assert!(out(&o).is_empty());
}

#[test]
fn every_command_of_the_usage_line_has_help() {
    let usage = err(&run(&[]));
    let line = usage
        .lines()
        .find(|l| l.starts_with("usage:"))
        .expect("a usage line");
    // The commands are the `|`-separated words between the output flags
    // and the first option of the rest.
    let from = line.find("yaml] ").expect("the output flags") + "yaml] ".len();
    let list = &line[from..line.find(" [--wait").expect("the options")];
    let commands: Vec<&str> = list.split('|').collect();
    assert!(commands.len() > 80, "{} commands", commands.len());
    for c in commands {
        let words: Vec<&str> = c.split(' ').collect();
        let mut args = vec!["help"];
        args.extend(words);
        let o = run(&args);
        assert_eq!(o.status.code(), Some(0), "no help for {c}: {}", err(&o));
        assert!(out(&o).contains(c), "help for {c} doesn't name it");
    }
}

#[test]
fn an_unknown_option_is_named() {
    let o = run(&["create-volume", "--bogus"]);
    assert_eq!(o.status.code(), Some(2));
    let e = err(&o);
    assert!(e.contains("unknown argument --bogus"), "{e}");
    assert_eq!(
        e.lines().filter(|l| l.starts_with("usage:")).count(),
        1,
        "{e}"
    );
}

#[test]
fn a_bad_value_names_the_option() {
    // `--pool` takes a pool's number.
    let o = run(&["create-volume", "--size", "1G", "--pool", "vol"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("--pool"), "{}", err(&o));
}

#[test]
fn options_that_do_not_fit_the_command_point_to_its_help() {
    // No size, which `create-volume` needs.
    let o = run(&["create-volume", "--pool", "1"]);
    assert_eq!(o.status.code(), Some(2));
    let e = err(&o);
    assert!(e.contains("create-volume"), "{e}");
    assert!(e.contains("hyperstorctl help create-volume"), "{e}");
}

#[test]
fn a_missing_command_is_said() {
    let o = run(&["--json"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("no command given"), "{}", err(&o));
}
