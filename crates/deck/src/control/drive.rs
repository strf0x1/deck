//! Carrying out control commands on the live window: input becomes GPUI
//! events dispatched on the window, and `state` reads the Deck back as JSON.
//!
//! The design choice: input goes through `Window::dispatch_event`, the entry
//! point the platform layer itself calls for real keys and pointer events, so
//! hit-testing, hover, focus and the Deck's own listeners all run exactly as
//! they would for the user; nothing here reaches into Deck's state to fake a
//! result. Positions are window-relative logical pixels, as in layout.

use super::protocol::{self, Command, Mods, Point, Press};
use crate::ui::root::Deck;
use crate::ui::session::{Entry, Status};
use deck_acp::ToolContent;
use gpui::{
    AnyWindowHandle, AppContext as _, AsyncApp, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, WindowHandle, point, px,
};
use serde_json::{Value, json};
use std::time::Duration;

/// Roughly one frame: drags pause this long between moves so they animate.
const FRAME: Duration = Duration::from_millis(16);
const SETTLE_LIMIT: Duration = Duration::from_secs(2);

/// Carry out `cmd` on `deck`'s window and say how it went.
pub async fn run(cmd: Command, deck: WindowHandle<Deck>, cx: &mut AsyncApp) -> Value {
    match execute(cmd, deck, cx).await {
        Ok(v) => protocol::ok(v),
        Err(e) => protocol::error(format!("{e:#}")),
    }
}

async fn execute(cmd: Command, deck: WindowHandle<Deck>, cx: &mut AsyncApp) -> anyhow::Result<Value> {
    let window: AnyWindowHandle = deck.into();
    match cmd {
        Command::Keys(presses) => {
            for p in &presses {
                let keystroke = keystroke(p);
                dispatch(
                    window,
                    cx,
                    PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: keystroke.clone(),
                        is_held: false,
                        prefer_character_input: false,
                    }),
                )?;
                dispatch(window, cx, PlatformInput::KeyUp(KeyUpEvent { keystroke }))?;
            }
            Ok(json!({ "keys": presses.len() }))
        }
        Command::Move(p) => {
            pointer_move(window, cx, p, None)?;
            Ok(json!({}))
        }
        Command::Click(p) => {
            click(window, cx, p)?;
            Ok(json!({}))
        }
        Command::Drag { from, to, steps } => {
            pointer_move(window, cx, from, None)?;
            dispatch(window, cx, mouse_down(from))?;
            for i in 1..=steps {
                cx.background_executor().timer(FRAME).await;
                let t = i as f32 / steps as f32;
                let at = Point { x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t };
                pointer_move(window, cx, at, Some(MouseButton::Left))?;
            }
            dispatch(window, cx, mouse_up(to))?;
            Ok(json!({}))
        }
        Command::HoverRail => {
            let (x, y) = Deck::rail_item_center(0);
            pointer_move(window, cx, Point { x, y }, None)?;
            Ok(json!({}))
        }
        Command::SelectRail(i) => {
            anyhow::ensure!(i < crate::ui::rail::ITEMS.len(), "no rail item {i}");
            let (x, y) = Deck::rail_item_center(i);
            click(window, cx, Point { x, y })?;
            Ok(json!({}))
        }
        Command::Wait(ms) => {
            cx.background_executor().timer(Duration::from_millis(ms)).await;
            Ok(json!({}))
        }
        Command::Settle => {
            let mut waited = Duration::ZERO;
            while deck.read_with(cx, |d, _| d.animating())? {
                if waited >= SETTLE_LIMIT {
                    return Ok(json!({ "settled": false }));
                }
                cx.background_executor().timer(FRAME).await;
                waited += FRAME;
            }
            Ok(json!({ "settled": true }))
        }
        Command::State => deck.read_with(cx, |d, _| state(d)),
        Command::Geometry => cx.update_window(window, |_, window, _| {
            let b = window.bounds();
            let v = window.viewport_size();
            json!({
                "pid": std::process::id(),
                "bounds": { "x": f32::from(b.origin.x), "y": f32::from(b.origin.y),
                            "w": f32::from(b.size.width), "h": f32::from(b.size.height) },
                "viewport": { "w": f32::from(v.width), "h": f32::from(v.height) },
                "scale": window.scale_factor(),
            })
        }),
    }
}

fn dispatch(window: AnyWindowHandle, cx: &mut AsyncApp, ev: PlatformInput) -> anyhow::Result<()> {
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(ev, cx);
    })
}

fn pointer_move(window: AnyWindowHandle, cx: &mut AsyncApp, p: Point, held: Option<MouseButton>) -> anyhow::Result<()> {
    dispatch(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: point(px(p.x), px(p.y)),
            pressed_button: held,
            modifiers: Modifiers::none(),
        }),
    )
}

fn click(window: AnyWindowHandle, cx: &mut AsyncApp, p: Point) -> anyhow::Result<()> {
    pointer_move(window, cx, p, None)?;
    dispatch(window, cx, mouse_down(p))?;
    dispatch(window, cx, mouse_up(p))
}

fn mouse_down(p: Point) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Left,
        position: point(px(p.x), px(p.y)),
        modifiers: Modifiers::none(),
        click_count: 1,
        first_mouse: false,
    })
}

fn mouse_up(p: Point) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position: point(px(p.x), px(p.y)),
        modifiers: Modifiers::none(),
        click_count: 1,
    })
}

fn keystroke(p: &Press) -> Keystroke {
    let Mods { ctrl, alt, shift, platform } = p.mods;
    Keystroke {
        modifiers: Modifiers { control: ctrl, alt, shift, platform, ..Modifiers::default() },
        key: p.key.clone(),
        key_char: p.ch.clone(),
    }
}

/// What an agent needs to check the window did the right thing.
pub fn state(d: &Deck) -> Value {
    let board = d.board();
    let rail = d.rail();
    let s = d.session();
    let status = match &s.status {
        Status::Connecting => json!("connecting"),
        Status::Ready => json!("ready"),
        Status::Waiting => json!("waiting"),
        Status::Working => json!("working"),
        Status::Detached => json!("detached"),
        Status::Down(e) => json!({ "down": e }),
    };
    let entries: Vec<Value> = s
        .entries
        .iter()
        .map(|e| match e {
            Entry::You(t) => json!({ "you": t }),
            Entry::Agent(t) => json!({ "agent": t }),
            Entry::Thought(t) => json!({ "thought": t }),
            Entry::Tool(tool) => {
                let c = &tool.call;
                json!({
                    "tool": c.title,
                    "id": c.id,
                    "kind": format!("{:?}", c.kind).to_lowercase(),
                    "status": format!("{:?}", c.status).to_lowercase(),
                    "diffs": c.content.iter().filter(|x| matches!(x, ToolContent::Diff { .. })).count(),
                    "locations": c.locations.iter().map(|l| &l.path).collect::<Vec<_>>(),
                })
            }
            Entry::Note(t) => json!({ "note": t }),
        })
        .collect();
    json!({
        "board": {
            "order": board.order(),
            "cards": board.cards().iter().map(|c| json!({
                "id": c.lane.id,
                "title": c.lane.title,
                "state": format!("{:?}", c.lane.state).to_lowercase(),
                "detail": c.lane.detail,
                "chip": c.lane.chip.as_ref().map(|(dot, label)| json!({ "dot": dot.label(), "label": label })),
                "y": c.y.pos,
                "lift": c.lift.pos,
            })).collect::<Vec<_>>(),
            "dragging": board.dragging(),
        },
        "publication": d.open_publication().map(|(task, _)| task.to_string()),
        "publish_notice": d.publish_notice(),
        "rail": {
            "width": rail.width.pos,
            "active": rail.active(),
            "pinned": rail.pinned(),
            "expanded": rail.expanded(),
        },
        "session": {
            "agent": s.agent,
            "status": status,
            "entries": entries,
            "ask": s.ask.as_ref().map(|a| json!({
                "title": a.title,
                "options": a.options.iter().map(|o| json!({ "id": o.id, "name": o.name, "kind": o.kind })).collect::<Vec<_>>(),
            })),
        },
        "approval": d.boundary().pending().map(|item| json!({
            "id": item.id,
            "task": item.task,
            "gateway": item.gateway,
            "operation": item.operation,
            "destination": item.destination,
            "credential": item.credential,
            "state": format!("{:?}", item.state).to_lowercase(),
            "waiting": item.waiters > 0,
        })),
        "draft": d.draft(),
        "access": {
            "open": d.access().is_open(),
            "grants": d.access().grants().iter().map(|grant| json!({
                "id": grant.id, "gateway": grant.gateway, "operation": grant.operation,
                "destination": grant.destination, "scope": grant.scope, "expires_ms": grant.expires_ms,
            })).collect::<Vec<_>>(),
            "confirming": d.access().confirming().map(|target| target.label()),
            "busy": d.access().busy(),
        },
        "animating": d.animating(),
    })
}
