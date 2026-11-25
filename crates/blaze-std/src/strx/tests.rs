use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
use blaze_sim::{Broke, Config, Tape};

use super::*;

#[test]
fn case_conversion_edge_cases() {
    let h = heap();
    let alloc = h.alloc();

    assert_eq!(to_snake_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("http_server"));
    assert_eq!(to_screaming_snake_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("HTTP_SERVER"));
    assert_eq!(to_kebab_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("http-server"));
    assert_eq!(to_camel_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("httpServer"));
    assert_eq!(to_pascal_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("HttpServer"));
    assert_eq!(to_title_case_in("HTTPServer", alloc).as_ref().map(OwnString::as_str), Ok("Http Server"));

    assert_eq!(to_snake_case_in("parseHTML5Doc", alloc).as_ref().map(OwnString::as_str), Ok("parse_html5_doc"));
    assert_eq!(to_camel_case_in("parseHTML5Doc", alloc).as_ref().map(OwnString::as_str), Ok("parseHtml5Doc"));
    assert_eq!(to_pascal_case_in("parseHTML5Doc", alloc).as_ref().map(OwnString::as_str), Ok("ParseHtml5Doc"));

    assert_eq!(to_snake_case_in("getURLPath", alloc).as_ref().map(OwnString::as_str), Ok("get_url_path"));
    assert_eq!(to_kebab_case_in("getURLPath", alloc).as_ref().map(OwnString::as_str), Ok("get-url-path"));
    assert_eq!(to_title_case_in("getURLPath", alloc).as_ref().map(OwnString::as_str), Ok("Get Url Path"));

    assert_eq!(to_snake_case_in("already_snake", alloc).as_ref().map(OwnString::as_str), Ok("already_snake"));
    assert_eq!(to_camel_case_in("already_snake", alloc).as_ref().map(OwnString::as_str), Ok("alreadySnake"));
    assert_eq!(to_pascal_case_in("foo--bar", alloc).as_ref().map(OwnString::as_str), Ok("FooBar"));
    assert_eq!(to_snake_case_in("foo--bar", alloc).as_ref().map(OwnString::as_str), Ok("foo_bar"));
    assert_eq!(to_snake_case_in("XMLHttpRequest", alloc).as_ref().map(OwnString::as_str), Ok("xml_http_request"));
    assert_eq!(to_snake_case_in("___---", alloc).as_ref().map(OwnString::as_str), Ok(""));
}

#[test]
fn slugify_diacritics_and_separators() {
    let h = heap();
    let alloc = h.alloc();

    assert_eq!(
        slugify_in("Café & Crème Brûlée!", '-', alloc).as_ref().map(OwnString::as_str),
        Ok("cafe-creme-brulee")
    );
    assert_eq!(
        slugify_in("  --- Hello, World! 2026 ---  ", '_', alloc)
            .as_ref()
            .map(OwnString::as_str),
        Ok("hello_world_2026")
    );
    assert_eq!(
        slugify_in("Über Straße & Cœur", '-', alloc).as_ref().map(OwnString::as_str),
        Ok("uber-strasse-coeur")
    );
    assert_eq!(
        slugify_in("!!!", '-', alloc).as_ref().map(OwnString::as_str),
        Ok("")
    );
}

#[test]
fn natural_cmp_ordering_and_huge_numbers() {
    let h = heap();
    let alloc = h.alloc();
    let mut items = OwnVec::new_in(alloc);
    for s in ["v100", "v2", "v10", "v1", "v1.10", "v1.2", "v1.1"] {
        items.push(s);
    }
    items.sort_unstable_by(|a, b| natural_cmp(a, b));
    assert_eq!(items.as_slice(), &["v1", "v1.1", "v1.2", "v1.10", "v2", "v10", "v100"]);

    assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
    assert_eq!(natural_cmp("0", "00"), Ordering::Less);
    assert_eq!(natural_cmp("00", "1"), Ordering::Less);
    assert_eq!(natural_cmp("1", "01"), Ordering::Less);

    // 100-digit integers never overflow u64.
    let mut huge_a = OwnString::new_in(alloc);
    let mut huge_b = OwnString::new_in(alloc);
    let mut huge_c = OwnString::new_in(alloc);
    huge_a.push('n');
    huge_b.push('n');
    huge_c.push_str("n1");
    for _ in 0..99 {
        huge_a.push('9');
        huge_b.push('9');
        huge_c.push('0');
    }
    huge_a.push('8');
    huge_b.push('9');
    huge_c.push('0');
    assert_eq!(natural_cmp(huge_a.as_str(), huge_b.as_str()), Ordering::Less);
    assert_eq!(natural_cmp(huge_b.as_str(), huge_c.as_str()), Ordering::Less);
    assert_eq!(natural_cmp(huge_a.as_str(), huge_c.as_str()), Ordering::Less);
}

#[test]
fn dedent_and_indent_preserve_blank_lines_and_endings() {
    let h = heap();
    let alloc = h.alloc();

    let src = "    fn main() {\n        println!(\"hi\");\n    \n    }\n";
    let dedented = dedent_in(src, alloc);
    assert_eq!(
        dedented.as_ref().map(OwnString::as_str),
        Ok("fn main() {\n    println!(\"hi\");\n\n}\n")
    );

    let crlf = "\t\ta\r\n\t\t\r\n\tb";
    assert_eq!(dedent_in(crlf, alloc).as_ref().map(OwnString::as_str), Ok("\ta\r\n\r\nb"));

    let indented = indent_in("a\n\nb\r\nc", "  ", alloc);
    assert_eq!(indented.as_ref().map(OwnString::as_str), Ok("  a\n\n  b\r\n  c"));

    if let Ok(ind) = indented {
        assert_eq!(dedent_in(ind.as_str(), alloc).as_ref().map(OwnString::as_str), Ok("a\n\nb\r\nc"));
    }
}

#[test]
fn truncate_width_ansi_and_cjk() {
    let h = heap();
    let alloc = h.alloc();

    // Fits verbatim without truncation or added reset.
    let short = "\x1b[31mhi\x1b[0m";
    assert_eq!(
        truncate_width_in(short, 5, "…", alloc).as_ref().map(OwnString::as_str),
        Ok(short)
    );

    // Truncated ANSI string closes color with \x1b[0m.
    let colored = "\x1b[31mhello world\x1b[0m";
    let cut = truncate_width_in(colored, 6, "…", alloc);
    assert_eq!(cut.as_ref().map(OwnString::as_str), Ok("\x1b[31mhello…\x1b[0m"));
    if let Ok(s) = cut {
        assert_eq!(visible_width(s.as_str()), 6);
    }

    // Wide CJK chars never split mid-glyph.
    let cjk = "\x1b[32m日本語テスト\x1b[0m";
    let cut_cjk = truncate_width_in(cjk, 5, "…", alloc);
    assert_eq!(cut_cjk.as_ref().map(OwnString::as_str), Ok("\x1b[32m日本…\x1b[0m"));
    if let Ok(s) = cut_cjk {
        assert_eq!(visible_width(s.as_str()), 5);
    }

    // Odd width with 2-cell CJK leaves width 3 <= 4.
    let odd_cjk = truncate_width_in("日本語", 4, "…", alloc);
    assert_eq!(odd_cjk.as_ref().map(OwnString::as_str), Ok("日…"));
    if let Ok(s) = odd_cjk {
        assert!(visible_width(s.as_str()) <= 4);
    }

    // Edge cases: max_width == 0 and ellipsis wider than max_width.
    assert_eq!(truncate_width_in("hello", 0, "…", alloc).as_ref().map(OwnString::as_str), Ok(""));
    assert_eq!(truncate_width_in("hello", 2, "...", alloc).as_ref().map(OwnString::as_str), Ok(".."));
}

fn draw_natural_str<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
    const PIECES: &[&str] = &[
        "a", "b", "0", "00", "1", "01", "2", "10", "99", "100", "-", ".", "/", "_", ":",
    ];
    let count = tape.int_in(0usize..=5, "pieces");
    let mut s = OwnString::new_in(alloc);
    for _ in 0..count {
        let p = tape.pick(PIECES, "piece").copied().unwrap_or("0");
        s.push_str(p);
    }
    s
}

/// The invariant: `natural_cmp` is a strict total order (reflexive, antisymmetric, transitive) and orders embedded decimal integers numerically.
#[test]
fn property_natural_cmp_strict_total_order() {
    let h = heap();
    blaze_sim::check(Config::cases(256), |tape| {
        let alloc = h.alloc();
        let a = draw_natural_str(tape, alloc);
        let b = draw_natural_str(tape, alloc);
        let c = draw_natural_str(tape, alloc);

        // Reflexivity.
        if natural_cmp(a.as_str(), a.as_str()) != Ordering::Equal {
            return Err(Broke::Oracle(format_in!(alloc, "reflexivity failed for {:?}", a.as_str()).as_str().into()));
        }
        // Antisymmetry & equality identity.
        let ab = natural_cmp(a.as_str(), b.as_str());
        let ba = natural_cmp(b.as_str(), a.as_str());
        if ab != ba.reverse() {
            return Err(Broke::Oracle(
                format_in!(alloc, "antisymmetry failed for {:?} vs {:?}", a.as_str(), b.as_str())
                    .as_str()
                    .into(),
            ));
        }
        if (ab == Ordering::Equal) != (a.as_str() == b.as_str()) {
            return Err(Broke::Oracle(
                format_in!(alloc, "equality identity failed for {:?} vs {:?}", a.as_str(), b.as_str())
                    .as_str()
                    .into(),
            ));
        }

        // Transitivity across all 3 pairs.
        let bc = natural_cmp(b.as_str(), c.as_str());
        let ac = natural_cmp(a.as_str(), c.as_str());
        if ab == Ordering::Less && bc == Ordering::Less && ac != Ordering::Less {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "transitivity failed: {:?} < {:?} < {:?}",
                    a.as_str(),
                    b.as_str(),
                    c.as_str()
                )
                .as_str()
                .into(),
            ));
        }
        if ab == Ordering::Greater && bc == Ordering::Greater && ac != Ordering::Greater {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "reverse transitivity failed: {:?} > {:?} > {:?}",
                    a.as_str(),
                    b.as_str(),
                    c.as_str()
                )
                .as_str()
                .into(),
            ));
        }

        // Numeric agreement on embedded integers.
        let x = tape.int_in(0u64..=500u64, "x");
        let y = tape.int_in(0u64..=500u64, "y");
        let sx = format_in!(alloc, "item{x}.txt");
        let sy = format_in!(alloc, "item{y}.txt");
        if natural_cmp(sx.as_str(), sy.as_str()) != x.cmp(&y) {
            return Err(Broke::Oracle(
                format_in!(alloc, "numeric mismatch for {sx} vs {sy}").as_str().into(),
            ));
        }
        Ok(())
    });
}

/// The invariant: `truncate_width_in` always produces a string with `visible_width <= max_width` and closes any ANSI style emitted before truncating.
#[test]
fn property_truncate_width_respects_bound() {
    let h = heap();
    const CHUNKS: &[&str] = &[
        "a",
        "hello",
        "日",
        "本語",
        "e\u{0301}",
        "\x1b[31m",
        "\x1b[1;34m",
        "\x1b[0m",
        " ",
    ];
    const ELLIPSES: &[&str] = &["…", "...", "", "——"];

    blaze_sim::check(Config::cases(256), |tape| {
        let alloc = h.alloc();
        let count = tape.int_in(0usize..=10, "chunks");
        let mut input = OwnString::new_in(alloc);
        for _ in 0..count {
            let c = tape.pick(CHUNKS, "chunk").copied().unwrap_or("a");
            input.push_str(c);
        }
        let max_width = tape.int_in(0usize..=20, "max_width");
        let ellipsis = tape.pick(ELLIPSES, "ellipsis").copied().unwrap_or("…");

        let out = truncate_width_in(input.as_str(), max_width, ellipsis, alloc)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "truncate alloc: {e}").as_str().into()))?;
        let got_width = visible_width(out.as_str());
        if got_width > max_width {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "visible width {got_width} exceeded max_width {max_width} for {:?}",
                    out.as_str()
                )
                .as_str()
                .into(),
            ));
        }
        if visible_width(input.as_str()) <= max_width && out.as_str() != input.as_str() {
            return Err(Broke::Oracle(
                format_in!(alloc, "non-truncated input was altered").as_str().into(),
            ));
        }
        if visible_width(input.as_str()) > max_width
            && has_ansi(out.as_str())
            && !out.as_str().ends_with("\x1b[0m")
        {
            return Err(Broke::Oracle(
                format_in!(alloc, "truncated ANSI output leaked style: {:?}", out.as_str())
                    .as_str()
                    .into(),
            ));
        }
        Ok(())
    });
}

#[test]
fn the_oracle_catches_lexicographic_digit_bug() {
    let h = heap();
    blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
        let alloc = h.alloc();
        let x = tape.int_in(0u64..=200u64, "x");
        let y = tape.int_in(0u64..=200u64, "y");
        let a = format_in!(alloc, "file{x}");
        let b = format_in!(alloc, "file{y}");

        let got = natural_cmp_planted(a.as_str(), b.as_str(), plant);
        let want = x.cmp(&y);
        if got != want {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "natural_cmp({:?}, {:?}) returned {:?}, expected {:?}",
                    a.as_str(),
                    b.as_str(),
                    got,
                    want
                )
                .as_str()
                .into(),
            ));
        }
        Ok(())
    });
}
