//! The control channel's wire format: one JSON object per line in, one JSON
//! object per line out. A command is `{"cmd": "<name>", …arguments}`.
//!
//! The design choice: parsing turns text into plain [`Command`]s and key
//! presses into plain [`Press`]es, with no GPUI types, so the whole grammar
//! is unit-tested here; `drive` only maps them onto GPUI events.

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

/// One key going down, as GPUI names it: `key` is the key's name ("a",
/// "enter"), `ch` the text it types, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    pub key: String,
    pub mods: Mods,
    pub ch: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Key presses, from `type` (one per character) or `key` (one).
    Keys(Vec<Press>),
    Click(Point),
    Move(Point),
    Scroll {
        at: Point,
        delta: Point,
    },
    Drag {
        from: Point,
        to: Point,
        steps: u32,
    },
    /// Point at the rail so it opens.
    HoverRail,
    /// Click the rail item at this index.
    SelectRail(usize),
    /// Wait this many milliseconds, so frames can draw.
    Wait(u64),
    /// Wait until nothing animates (or a second passes).
    Settle,
    State,
    Geometry,
}

/// Keys that name themselves; anything else must be a single character.
const NAMED: [&str; 14] = [
    "enter",
    "escape",
    "backspace",
    "delete",
    "tab",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
];

pub fn parse(line: &str) -> Result<Command, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let cmd = v.get("cmd").and_then(Value::as_str).ok_or("missing \"cmd\"")?;
    Ok(match cmd {
        "type" => Command::Keys(typed(str_arg(&v, "text")?)),
        "key" => Command::Keys(vec![key(&v)?]),
        "click" => Command::Click(point(&v)?),
        "move" => Command::Move(point(&v)?),
        "scroll" => Command::Scroll { at: point(&v)?, delta: point(v.get("delta").ok_or("missing \"delta\"")?)? },
        "drag" => {
            let from = point(v.get("from").ok_or("missing \"from\"")?)?;
            let to = point(v.get("to").ok_or("missing \"to\"")?)?;
            let steps = v.get("steps").and_then(Value::as_u64).unwrap_or(12).clamp(1, 500) as u32;
            Command::Drag { from, to, steps }
        }
        "hover_rail" => Command::HoverRail,
        "select_rail" => {
            Command::SelectRail(v.get("index").and_then(Value::as_u64).ok_or("missing \"index\"")? as usize)
        }
        "wait" => Command::Wait(v.get("ms").and_then(Value::as_u64).ok_or("missing \"ms\"")?.min(10_000)),
        "settle" => Command::Settle,
        "state" => Command::State,
        "geometry" => Command::Geometry,
        other => return Err(format!("unknown cmd {other:?}")),
    })
}

fn str_arg<'a>(v: &'a Value, name: &str) -> Result<&'a str, String> {
    v.get(name).and_then(Value::as_str).ok_or_else(|| format!("missing \"{name}\""))
}

fn point(v: &Value) -> Result<Point, String> {
    let n =
        |k: &str| v.get(k).and_then(Value::as_f64).map(|f| f as f32).ok_or_else(|| format!("missing number \"{k}\""));
    Ok(Point { x: n("x")?, y: n("y")? })
}

/// What typing `text` presses: one key per character, newlines as enter.
pub fn typed(text: &str) -> Vec<Press> {
    text.chars()
        .map(|c| match c {
            '\n' => Press { key: "enter".into(), mods: Mods::default(), ch: None },
            ' ' => Press { key: "space".into(), mods: Mods::default(), ch: Some(" ".into()) },
            c => {
                let shift = c.is_uppercase();
                Press { key: c.to_lowercase().collect(), mods: Mods { shift, ..Mods::default() }, ch: Some(c.into()) }
            }
        })
        .collect()
}

fn key(v: &Value) -> Result<Press, String> {
    let name = str_arg(v, "key")?.to_lowercase();
    if !NAMED.contains(&name.as_str()) && name.chars().count() != 1 {
        return Err(format!("unknown key {name:?}"));
    }
    let mut mods = Mods::default();
    for m in v.get("modifiers").and_then(Value::as_array).into_iter().flatten() {
        match m.as_str().unwrap_or_default() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            "super" | "cmd" | "platform" => mods.platform = true,
            other => return Err(format!("unknown modifier {other:?}")),
        }
    }
    let plain = !(mods.ctrl || mods.alt || mods.platform);
    let ch = match name.as_str() {
        "space" if plain => Some(" ".into()),
        n if plain && n.chars().count() == 1 => Some(if mods.shift { n.to_uppercase() } else { n.to_string() }),
        _ => None,
    };
    Ok(Press { key: name, mods, ch })
}

pub fn ok(fields: Value) -> Value {
    let mut out = json!({ "ok": true });
    if let (Some(o), Value::Object(f)) = (out.as_object_mut(), fields) {
        o.extend(f);
    }
    out
}

pub fn error(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "error": msg.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(key: &str, ch: Option<&str>, shift: bool) -> Press {
        Press { key: key.into(), mods: Mods { shift, ..Mods::default() }, ch: ch.map(Into::into) }
    }

    #[test]
    fn typing_presses_one_key_per_character() {
        assert_eq!(
            parse(r#"{"cmd":"type","text":"Hi 2\n"}"#),
            Ok(Command::Keys(vec![
                p("h", Some("H"), true),
                p("i", Some("i"), false),
                p("space", Some(" "), false),
                p("2", Some("2"), false),
                p("enter", None, false),
            ]))
        );
    }

    #[test]
    fn keys_take_modifiers_and_only_plain_keys_type_text() {
        assert_eq!(parse(r#"{"cmd":"key","key":"Enter"}"#), Ok(Command::Keys(vec![p("enter", None, false)])));
        assert_eq!(parse(r#"{"cmd":"key","key":"2"}"#), Ok(Command::Keys(vec![p("2", Some("2"), false)])));
        let Ok(Command::Keys(k)) = parse(r#"{"cmd":"key","key":"c","modifiers":["ctrl"]}"#) else { panic!() };
        assert_eq!(k[0].ch, None);
        assert!(k[0].mods.ctrl);
        assert!(parse(r#"{"cmd":"key","key":"c","modifiers":["hyper"]}"#).is_err());
        assert!(parse(r#"{"cmd":"key","key":"bogus"}"#).is_err());
    }

    #[test]
    fn pointer_commands_need_coordinates() {
        assert_eq!(parse(r#"{"cmd":"click","x":10,"y":20.5}"#), Ok(Command::Click(Point { x: 10., y: 20.5 })));
        assert!(parse(r#"{"cmd":"move","x":10}"#).is_err());
        assert_eq!(
            parse(r#"{"cmd":"drag","from":{"x":1,"y":2},"to":{"x":3,"y":4}}"#),
            Ok(Command::Drag { from: Point { x: 1., y: 2. }, to: Point { x: 3., y: 4. }, steps: 12 })
        );
        assert_eq!(
            parse(r#"{"cmd":"scroll","x":400,"y":300,"delta":{"x":0,"y":-240}}"#),
            Ok(Command::Scroll { at: Point { x: 400., y: 300. }, delta: Point { x: 0., y: -240. } })
        );
        assert!(parse(r#"{"cmd":"scroll","x":400,"y":300}"#).is_err());
    }

    #[test]
    fn bad_lines_say_what_is_wrong() {
        assert_eq!(parse(r#"{"cmd":"fly"}"#), Err("unknown cmd \"fly\"".into()));
        assert_eq!(parse("{}"), Err("missing \"cmd\"".into()));
        assert!(parse("nope").is_err_and(|e| e.starts_with("not JSON")));
    }

    #[test]
    fn replies_carry_ok() {
        assert_eq!(ok(json!({ "a": 1 })), json!({ "ok": true, "a": 1 }));
        assert_eq!(error("x"), json!({ "ok": false, "error": "x" }));
    }
}
