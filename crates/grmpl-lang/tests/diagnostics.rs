//! Diagnostics carry the line and column of what caused them: lexer and parser
//! errors point at the offending token (or the end of input), compile errors at
//! the declaration, column or view atom responsible, and the rendered form that
//! `Program::compile` returns quotes the line with a caret under the column.

use grmpl_lang::diagnostic::with_path;
use grmpl_lang::{parse, Diagnostic, Pos, Program};

fn parse_err(src: &str) -> Diagnostic {
    parse(src).expect_err("source should not parse")
}

fn compile_err(src: &str) -> String {
    Program::compile(src, 1).err().expect("source should not compile")
}

#[test]
fn a_missing_paren_points_at_the_token_found_instead() {
    let src = "rel foo(a: Ent\nview x() { foo(a) yield a }\n";
    let d = parse_err(src);
    assert_eq!(d.pos, Pos::new(2, 1));
    assert_eq!(d.msg, "expected `)`, found `view`");
}

#[test]
fn the_rendered_compile_error_quotes_the_line_with_a_caret() {
    let src = "rel foo(a: Ent\nview x() { foo(a) yield a }\n";
    assert_eq!(
        compile_err(src),
        "2:1: expected `)`, found `view`\n  |\n2 | view x() { foo(a) yield a }\n  | ^"
    );
    assert_eq!(
        parse_err(src).render(src, Some("bad.grmpl")).lines().next(),
        Some("bad.grmpl:2:1: expected `)`, found `view`")
    );
    assert!(with_path(&compile_err(src), "bad.grmpl").starts_with("bad.grmpl:2:1: expected"));
}

#[test]
fn an_unterminated_string_points_at_its_opening_quote() {
    let d = parse_err("rel said(who, what)\nform f {\n    \"take name -> Take(name)\n}\n");
    assert_eq!(d.pos, Pos::new(3, 5));
    assert_eq!(d.msg, "unterminated string literal");
}

#[test]
fn an_unknown_character_points_at_itself() {
    let d = parse_err("rel a(x)\nrel b(y) @ rel c(z)");
    assert_eq!(d.pos, Pos::new(2, 10));
    assert_eq!(d.msg, "unexpected character `@`");
}

#[test]
fn running_out_of_input_points_past_the_last_token() {
    let d = parse_err("rel located(thing, \n// trailing comment\n");
    assert_eq!(d.pos, Pos::new(1, 19));
    assert_eq!(d.msg, "expected identifier, found end of input");
}

#[test]
fn an_error_deep_in_a_file_reports_its_line_and_column() {
    let src = "rel located(thing, place)\n\
               view here(p) {\n    \
                   located(p room)\n    \
                   yield room\n\
               }\n";
    let d = parse_err(src);
    assert_eq!(d.pos, Pos::new(3, 15));
    assert_eq!(d.msg, "expected `)`, found `room`");
}

#[test]
fn columns_count_chars_not_bytes() {
    let d = parse_err("rel ñame(x)\nrel é(\"ü\")");
    assert_eq!(d.pos, Pos::new(2, 7));
    assert_eq!(d.msg, "expected identifier, found `\"ü\"`");
}

#[test]
fn an_undeclared_relation_points_at_the_atom_using_it() {
    let src = "rel located(thing, place)\n\
               \n\
               view here(p) {\n    \
                   located(p, r)\n    \
                   named(r, n)\n    \
                   yield n\n\
               }\n";
    let err = compile_err(src);
    assert!(
        err.starts_with("5:5: view `here` uses undeclared relation `named`\n"),
        "{err}"
    );
    assert!(err.ends_with("5 |     named(r, n)\n  |     ^"), "{err}");
}

#[test]
fn an_arity_mismatch_points_at_the_atom() {
    let src = "rel located(thing, place)\nview v() { located(t) yield t }\n";
    assert!(compile_err(src).starts_with("2:12: `located` has arity 2 but was used with 1 args"));
}

#[test]
fn declaration_errors_point_at_the_declaration_or_column() {
    let twice = "rel a(x)\nview v() { a(x) yield x }\nrel a(y)\n";
    assert!(compile_err(twice).starts_with("3:1: relation `a` declared twice"));

    let column = "rel a(x: Int,\n      x: Text)\n";
    assert!(compile_err(column).starts_with("2:7: relation `a` has a duplicate column `x`"));

    let ty = "rel a(x: Integer)\n";
    assert!(compile_err(ty).starts_with("1:7: relation `a` column `x` has unknown type `Integer`"));
}
