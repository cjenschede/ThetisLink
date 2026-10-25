// SPDX-License-Identifier: GPL-2.0-or-later
//! Reading Rust source as text, for the tests that pin what a file does or
//! does not contain. The one reader that takes the test code out of a source
//! file: the core's measurement-line test and the app's one-logger test both
//! use it (review of 299: a second, simpler reader in the app missed an
//! install behind a `#[cfg(test)] use ...;` that this one already handled).
//! Only tests call it; it is public so the other crates' tests can.

/// The source without its test code: every line that is exactly
/// `#[cfg(test)]` takes the item under it out, attributes and all. The item
/// ends on the line where its brackets are closed again and that ends in `;`
/// or `}` (`fn h() { 1 }`, `use a::{` ... `};`, `const X = [` ... `];` -
/// review of 296), and at the latest at a `}` at the start of a line.
/// Production code after a `mod tests` is read (review of 295). Strings,
/// character literals and `//` comments are skipped while counting.
///
/// A line can end the item while a string still seems open: a raw string
/// with a `"` inside (`r#" " "#`) leaves `in_str` set, and before build 301
/// that let the item run on to the next `}` at the start of a line, the
/// production code in between unread (review of 300, N2). The price: an item
/// with a string over several lines, one of them ending in `;` or `}`, ends
/// too early and the test sees more. A bracket in a `/* */` comment can still
/// shift the count. An indented `#[cfg(test)]`, or one with a comment behind
/// it, stays in: the test sees more. Comments are kept in what comes back; a
/// comment that names what a test counts is counted.
pub fn production_code(src: &str) -> String {
    let mut out = String::new();
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if line != "#[cfg(test)]" {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let (mut depth, mut in_str) = (0i64, false);
        for item in lines.by_ref() {
            depth += bracket_change(item, &mut in_str);
            let end = item.trim_end();
            if item == "}" || (depth <= 0 && (end.ends_with(';') || end.ends_with('}'))) {
                break;
            }
        }
    }
    out
}

/// Opened minus closed brackets on a line, outside strings, character
/// literals and a `//` comment; `in_str` carries a string over to the next
/// line. An escaped character literal needs no arm of its own: none holds a
/// lone bracket (`'\u{7b}'` holds a pair).
fn bracket_change(line: &str, in_str: &mut bool) -> i64 {
    let bytes = line.as_bytes();
    let (mut change, mut i) = (0i64, 0usize);
    while i < bytes.len() {
        match (*in_str, bytes[i]) {
            (true, b'\\') => i += 1,
            (true, b'"') => *in_str = false,
            (true, _) => {}
            (false, b'"') => *in_str = true,
            (false, b'/') if bytes.get(i + 1) == Some(&b'/') => break,
            (false, b'\'') if bytes.get(i + 2) == Some(&b'\'') => i += 2,
            (false, b'{' | b'[' | b'(') => change += 1,
            (false, b'}' | b']' | b')') => change -= 1,
            (false, _) => {}
        }
        i += 1;
    }
    change
}

pub fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test code goes, production code after it stays (review of 295).
    #[test]
    fn production_code_after_the_tests_is_read() {
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { info!(\"x\"); }\n}\npub struct After;\n#[cfg(test)]\nuse foo::bar;\n#[cfg(test)]\nfn helper() {\n    info!(\"y\");\n}\nfn b() {}\n";
        assert_eq!(production_code(src), "fn a() {}\npub struct After;\nfn b() {}\n");
        // Items that close on the line they open, or with `};`/`];`, and an
        // attribute between: the next item stays (review of 296, P1/P2).
        let src = r##"#[cfg(test)]
fn h() -> u8 { 1 }
fn kept1() {}
#[cfg(test)]
const X: [u8; 2] = [
    1,
    2,
];
fn kept2() {}
#[cfg(test)]
use crate::{
    a,
};
fn kept3() {}
#[cfg(test)]
#[derive(Debug)]
struct S {
    a: &'static str,
}
fn kept4() {}
#[cfg(test)]
fn odd() -> (&'static str, char) {
    ("{ (", '}')
}
fn kept5() {}
#[cfg(test)]
const S: &str = "{";
fn kept6() {}
#[cfg(test)]
const C: char = '{';
fn kept7() {}
#[cfg(test)]
use crate::{
    a, // see foo(
};
fn kept8() {}
#[cfg(test)]
const R: &str = r#" " "#;
fn kept9() {}
"##;
        // `kept6` to `kept8` end on `;`, so only the count can end them: the
        // string, the character and the comment are skipped (review of 297).
        assert_eq!(
            production_code(src),
            "fn kept1() {}\nfn kept2() {}\nfn kept3() {}\nfn kept4() {}\nfn kept5() {}\nfn kept6() {}\nfn kept7() {}\nfn kept8() {}\nfn kept9() {}\n"
        );
    }
}
