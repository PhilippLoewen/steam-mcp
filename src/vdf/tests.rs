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

#[test]
fn parses_binary_values() {
    assert_eq!(parse_binary_vdf(&[]).unwrap(), VdfObject::new());
    assert_eq!(parse_binary_vdf(&buf(&[&[1], b"name\0Test Game\0", &[8]])).unwrap(), obj(&[("name", s("Test Game"))]));
    let n = 365_360u32.to_le_bytes();
    assert_eq!(parse_binary_vdf(&buf(&[&[2], b"appid\0", &n, &[8]])).unwrap(), obj(&[("appid", s("365360"))]));
    let max = u32::MAX.to_le_bytes();
    assert_eq!(parse_binary_vdf(&buf(&[&[2], b"max\0", &max, &[8]])).unwrap(), obj(&[("max", s("4294967295"))]));
    assert_eq!(
        parse_binary_vdf(&buf(&[&[1], b"key\0first\0", &[1], b"key\0second\0", &[8]])).unwrap(),
        obj(&[("key", s("second"))])
    );
}

#[test]
fn parses_nested_binary_objects() {
    let a = 2_000_000_000u32.to_le_bytes();
    let data =
        buf(&[&[0], b"shortcuts\0", &[0], b"0\0", &[1], b"AppName\0Heroic Game\0", &[2], b"appid\0", &a, &[8, 8, 8]]);
    let parsed = parse_binary_vdf(&data).unwrap();
    let entry = parsed.navigate(&["shortcuts", "0"]).unwrap();
    assert_eq!(entry.str_of(&["AppName"]), Some("Heroic Game"));
    assert_eq!(entry.str_of(&["appid"]), Some("2000000000"));
}

#[test]
fn rejects_unknown_binary_types() {
    let err = parse_binary_vdf(&[7, b'k', 0]).unwrap_err();
    assert_eq!(err.to_string(), "Unknown binary VDF type 0x07 at offset 2");
}

#[test]
fn binary_round_trips() {
    let tags = o(&[("0", s("Favorite")), ("1", s("RPG"))]);
    let doc = obj(&[(
        "shortcuts",
        o(&[("0", o(&[("appid", s("3000000000")), ("AppName", s("Game")), ("LaunchOptions", s("")), ("tags", tags)]))]),
    )]);
    let bytes = serialize_binary_vdf(&doc);
    assert_eq!(bytes.last(), Some(&8));
    assert_eq!(parse_binary_vdf(&bytes).unwrap(), doc);
    // "3000000000" fits a u32 and is written as one.
    assert!(bytes.windows(7).any(|w| w == b"\x02appid\0"));
}

#[test]
fn parse_int_matches_js() {
    assert_eq!(parse_int("123"), Some(123));
    assert_eq!(parse_int("  42abc"), Some(42));
    assert_eq!(parse_int("-7"), Some(-7));
    assert_eq!(parse_int(""), None);
    assert_eq!(parse_int("abc"), None);
}
