use super::*;

fn obj(entries: &[(&str, VdfValue)]) -> VdfObject {
    entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn s(v: &str) -> VdfValue {
    VdfValue::Str(v.to_string())
}

fn o(entries: &[(&str, VdfValue)]) -> VdfValue {
    VdfValue::Obj(obj(entries))
}

#[test]
fn parses_pairs_nesting_and_empty_objects() {
    assert_eq!(parse_vdf("\"key\"\t\t\"value\""), obj(&[("key", s("value"))]));
    assert_eq!(
        parse_vdf("\n  \"a\" \"1\"\n  \"b\" \"2\"\n  \"c\" \"3\"\n"),
        obj(&[("a", s("1")), ("b", s("2")), ("c", s("3"))])
    );
    assert_eq!(
        parse_vdf("\"outer\"\n{\n  \"inner\"\n  {\n    \"key\" \"val\"\n  }\n}\n"),
        obj(&[("outer", o(&[("inner", o(&[("key", s("val"))]))]))])
    );
    assert_eq!(parse_vdf("\"SharedDepots\" {}"), obj(&[("SharedDepots", o(&[]))]));
}

#[test]
fn duplicate_keys_keep_the_last_value() {
    assert_eq!(parse_vdf("\"key\" \"first\"\n\"key\" \"second\""), obj(&[("key", s("second"))]));
}

#[test]
fn handles_escapes_comments_and_unquoted_tokens() {
    assert_eq!(parse_vdf(r#""path"		"C:\\Program Files\\Steam""#), obj(&[("path", s(r"C:\Program Files\Steam"))]));
    assert_eq!(parse_vdf(r#""msg"		"He said \"hello\"""#), obj(&[("msg", s("He said \"hello\""))]));
    assert_eq!(parse_vdf(r#""text"		"line1\nline2\ttab""#), obj(&[("text", s("line1\nline2\ttab"))]));
    assert_eq!(parse_vdf(r#""p" "C:\Games""#), obj(&[("p", s(r"C:\Games"))]));
    assert_eq!(parse_vdf("// comment\n\"key\" \"value\"\n// another\n"), obj(&[("key", s("value"))]));
    assert_eq!(parse_vdf("key value"), obj(&[("key", s("value"))]));
    assert_eq!(parse_vdf(""), VdfObject::new());
    assert_eq!(parse_vdf("   \n\t\n  "), VdfObject::new());
}

#[test]
fn parses_an_appmanifest() {
    let input = "\"AppState\"\n{\n\t\"appid\"\t\t\"365360\"\n\t\"name\"\t\t\"Battle Brothers\"\n\t\"InstalledDepots\"\n\t{\n\t\t\"365361\"\n\t\t{\n\t\t\t\"manifest\"\t\t\"4185531896397071155\"\n\t\t}\n\t}\n\t\"SharedDepots\"\t\t{}\n\t\"UserConfig\"\n\t{\n\t\t\"language\"\t\t\"english\"\n\t}\n}";
    let parsed = parse_vdf(input);
    let state = parsed["AppState"].as_obj().unwrap();
    assert_eq!(state.str_of(&["appid"]), Some("365360"));
    assert_eq!(state.str_of(&["name"]), Some("Battle Brothers"));
    let depot = state.navigate(&["InstalledDepots", "365361"]).unwrap();
    assert_eq!(depot.str_of(&["manifest"]), Some("4185531896397071155"));
    assert_eq!(state["SharedDepots"], o(&[]));
    assert_eq!(state.navigate(&["userconfig"]).unwrap().str_of(&["language"]), Some("english"));
}

#[test]
fn serializes_with_tabs_escapes_and_indent() {
    let out = serialize_vdf(&obj(&[("appid", s("365360")), ("name", s("Test Game"))]), 0);
    assert_eq!(out, "\"appid\"\t\t\"365360\"\n\"name\"\t\t\"Test Game\"\n");

    let nested =
        obj(&[("AppState", o(&[("appid", s("123")), ("Depots", o(&[("456", o(&[("manifest", s("789"))]))]))]))]);
    let out = serialize_vdf(&nested, 0);
    assert!(out.contains("\t\t\t\"manifest\"\t\t\"789\"\n"));
    assert_eq!(parse_vdf(&out), nested);

    let out = serialize_vdf(&obj(&[("path", s(r"C:\Program Files\Steam")), ("msg", s("say \"hi\""))]), 0);
    assert!(out.contains(r"C:\\Program Files\\Steam"));
    assert!(out.contains(r#"say \"hi\""#));

    assert!(serialize_vdf(&obj(&[("SharedDepots", o(&[]))]), 0).contains("{\n}"));
    assert!(serialize_vdf(&obj(&[("key", s("val"))]), 2).starts_with("\t\t\"key\""));
}

#[test]
fn js_entries_puts_index_keys_first() {
    let o = obj(&[("b", s("1")), ("10", s("2")), ("2", s("3")), ("01", s("4")), ("a", s("5"))]);
    let keys: Vec<&str> = js_entries(&o).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["2", "10", "b", "01", "a"]);
}

fn buf(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn bin(entries: &[(&str, BinValue)]) -> BinObject {
    entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn bs(v: &str) -> BinValue {
    BinValue::Str(v.to_string())
}

fn bo(entries: &[(&str, BinValue)]) -> BinValue {
    BinValue::Obj(bin(entries))
}

#[test]
fn parses_binary_values() {
    assert_eq!(parse_binary_vdf(&[]).unwrap(), BinObject::new());
    assert_eq!(parse_binary_vdf(&buf(&[&[1], b"name\0Test Game\0", &[8]])).unwrap(), bin(&[("name", bs("Test Game"))]));
    let n = 365_360u32.to_le_bytes();
    assert_eq!(
        parse_binary_vdf(&buf(&[&[2], b"appid\0", &n, &[8]])).unwrap(),
        bin(&[("appid", BinValue::Uint(365_360))])
    );
    let max = u32::MAX.to_le_bytes();
    assert_eq!(
        parse_binary_vdf(&buf(&[&[2], b"max\0", &max, &[8]])).unwrap(),
        bin(&[("max", BinValue::Uint(u32::MAX))])
    );
    assert_eq!(
        parse_binary_vdf(&buf(&[&[1], b"key\0first\0", &[1], b"key\0second\0", &[8]])).unwrap(),
        bin(&[("key", bs("second"))])
    );
}

#[test]
fn parses_nested_binary_objects() {
    let a = 2_000_000_000u32.to_le_bytes();
    let data =
        buf(&[&[0], b"shortcuts\0", &[0], b"0\0", &[1], b"AppName\0Heroic Game\0", &[2], b"appid\0", &a, &[8, 8, 8]]);
    let parsed = parse_binary_vdf(&data).unwrap();
    let entry = parsed["shortcuts"].as_obj().unwrap()["0"].as_obj().unwrap();
    assert_eq!(entry["AppName"].text().as_deref(), Some("Heroic Game"));
    assert_eq!(entry["appid"].text().as_deref(), Some("2000000000"));
}

#[test]
fn rejects_unknown_binary_types() {
    let err = parse_binary_vdf(&[7, b'k', 0]).unwrap_err();
    assert_eq!(err.to_string(), "Unknown binary VDF type 0x07 at offset 2");
}

#[test]
fn rejects_truncated_binary_input() {
    // A nested object without its end tag.
    assert!(parse_binary_vdf(&buf(&[&[0], b"shortcuts\0", &[1], b"a\0b\0"])).is_err());
    // A string without its terminator.
    assert!(parse_binary_vdf(&buf(&[&[1], b"a\0unterminated"])).is_err());
    // A key without its terminator.
    assert!(parse_binary_vdf(&buf(&[&[1], b"key"])).is_err());
}

#[test]
fn binary_round_trips_with_types() {
    let tags = bo(&[("0", bs("Favorite")), ("1", bs("2048"))]);
    let doc = bin(&[(
        "shortcuts",
        bo(&[(
            "0",
            bo(&[
                ("appid", BinValue::Uint(3_000_000_000)),
                ("AppName", bs("007")),
                ("LaunchOptions", bs("")),
                ("tags", tags),
            ]),
        )]),
    )]);
    let bytes = serialize_binary_vdf(&doc).unwrap();
    assert_eq!(bytes.last(), Some(&8));
    assert_eq!(parse_binary_vdf(&bytes).unwrap(), doc);
    assert!(bytes.windows(7).any(|w| w == b"\x02appid\0"));
    // Digit strings stay strings.
    assert!(bytes.windows(13).any(|w| w == b"\x01AppName\x00007\x00"));
}

#[test]
fn binary_serializer_refuses_nul() {
    assert!(serialize_binary_vdf(&bin(&[("name", bs("a\0b"))])).is_err());
    assert!(serialize_binary_vdf(&bin(&[("na\0me", bs("ab"))])).is_err());
}

#[test]
fn deep_nesting_is_refused_without_overflowing() {
    let depth = 100_000;
    let mut data = Vec::new();
    for _ in 0..depth {
        data.extend_from_slice(b"\x00k\x00");
    }
    data.extend(std::iter::repeat_n(8u8, depth + 1));
    assert!(parse_binary_vdf(&data).unwrap_err().to_string().contains("nested deeper"));

    let text = "\"k\" {".repeat(depth) + &"}".repeat(depth);
    assert!(parse_vdf_strict(&text).unwrap_err().to_string().contains("nested deeper"));
    // The lenient parser stops at the limit instead of crashing.
    let _ = parse_vdf(&text);
}

#[test]
fn strict_parse_accepts_only_complete_input() {
    let good = "\"a\"\n{\n\t\"b\"\t\t\"c\"\n}\n";
    assert_eq!(parse_vdf_strict(good).unwrap(), parse_vdf(good));
    for bad in [
        "\"a\"\n{\n\t\"b\"\t\t\"c\"\n",
        "\"a\" \"unterminated",
        "\"a\" {}\n}",
        "\"a\"",
        "{ \"a\" \"b\" }",
        "\"a\" \"1\"\n\"a\" \"2\"",
    ] {
        assert!(parse_vdf_strict(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn parse_int_matches_js() {
    assert_eq!(parse_int("123"), Some(123));
    assert_eq!(parse_int("  42abc"), Some(42));
    assert_eq!(parse_int("-7"), Some(-7));
    assert_eq!(parse_int(""), None);
    assert_eq!(parse_int("abc"), None);
}
