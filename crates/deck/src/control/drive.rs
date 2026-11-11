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
use crate::ui::sidebar;
use deck_acp::ToolContent;
use gpui::{
    AnyWindowHandle, AppContext as _, AsyncApp, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, WindowHandle, point, px,
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
                // As the platform does: key down, then text the key handlers
                // left alone goes to the focused input handler, then key up.
                cx.update_window(window, |_, window, cx| {
                    window.dispatch_keystroke(keystroke.clone(), cx);
                })?;
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
            let (x, y) = deck.read_with(cx, |d, _| d.rail_item_center(0))?;
            pointer_move(window, cx, Point { x, y }, None)?;
            Ok(json!({}))
        }
        Command::SelectRail(i) => {
            let count = deck.read_with(cx, |d, _| d.projects().len())?;
            anyhow::ensure!(i < count, "no rail item {i}");
            let (x, y) = deck.read_with(cx, |d, _| d.rail_item_center(i))?;
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
                // Springs step on a draw, and the test clock alone does not draw.
                cx.update_window(window, |_, window, _| window.refresh())?;
                cx.background_executor().timer(FRAME).await;
                waited += FRAME;
            }
            Ok(json!({ "settled": true }))
        }
        Command::Scroll { at, delta } => {
            dispatch(
                window,
                cx,
                PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position: point(px(at.x), px(at.y)),
                    delta: gpui::ScrollDelta::Pixels(point(px(delta.x), px(delta.y))),
                    modifiers: Modifiers::none(),
                    touch_phase: gpui::TouchPhase::Moved,
                }),
            )?;
            Ok(json!({}))
        }
        Command::State => deck.read_with(cx, |d, cx| {
            let mut snapshot = state(d);
            let fonts = crate::ui::typography::CodeFont::ALL
                .into_iter()
                .map(|face| {
                    let report = crate::ui::fonts::resolve(face.family(), cx);
                    json!({ "requested": report.requested, "resolved": report.resolved })
                })
                .collect::<Vec<_>>();
            snapshot["typography"]["resolution"] = json!(fonts);
            snapshot
        }),
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
    let header = d.header_title();
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
        "main": if d.task_id().is_some() {
            "task"
        } else if d.project().is_some() {
            "project"
        } else {
            "none"
        },
        "header": {
            "title": header.title,
            "project": header.project,
            "detail": header.detail,
        },
        "needs_you": d.needs_you().iter().map(|item| json!({
            "task": item.task,
            "project": item.project,
            "reason": item.reason.text(),
        })).collect::<Vec<_>>(),
        "publication": d.open_publication().map(|(task, _)| task.to_string()),
        "publish_notice": d.publish_notice(),
        "new_session": {
            "creating": d.new_session_state().is_creating(),
            "error": d.new_session_state().error(),
        },
        "rail": {
            "width": rail.width.pos,
            "active": rail.active(),
            "pinned": rail.pinned(),
            "expanded": rail.expanded(),
        },
        "projects": d.projects().iter().map(|p| json!({ "id": p.id, "name": p.name })).collect::<Vec<_>>(),
        "project": d.project(),
        "attached": d.attached_ids(),
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
        "typography": {
            "open": d.appearance_open(),
            "percent": d.typography().percent(),
            "interface": d.typography().interface.family(),
            "output": d.typography().code.family(),
            "values": d.typography().code.value_family(),
            "icons": crate::ui::theme::ICONS,
        },
        "access": {
            "open": d.access().is_open(),
            "grants": d.access().grants().iter().map(|grant| json!({
                "id": grant.id, "gateway": grant.gateway, "operation": grant.operation,
                "destination": grant.destination, "scope": grant.scope, "expires_ms": grant.expires_ms,
            })).collect::<Vec<_>>(),
            "confirming": d.access().confirming().map(|target| target.label()),
            "busy": d.access().busy(),
        },
        "settings": settings_state(d),
        "sidebar": sidebar_state(d),
        "animating": d.animating(),
    })
}

/// The sidebar's rows, as `sidebar::rows` builds them, for a test to read.
fn sidebar_state(d: &Deck) -> Value {
    let names: std::collections::HashMap<&str, &str> =
        d.projects().iter().map(|project| (project.id.as_str(), project.name.as_str())).collect();
    let needs: Vec<sidebar::NeedIn> = d
        .needs_you()
        .iter()
        .map(|item| sidebar::NeedIn {
            task: item.task.clone(),
            project: item.project.clone(),
            project_name: names.get(item.project.as_str()).copied().unwrap_or(&item.project).to_string(),
            title: d.task_title(&item.task),
            word: item.reason.word(),
            text: item.reason.text(),
        })
        .collect();
    let projects: Vec<sidebar::ProjectIn> = d
        .projects()
        .iter()
        .map(|project| sidebar::ProjectIn {
            id: project.id.clone(),
            name: project.name.clone(),
            tasks: d
                .tasks()
                .iter()
                .filter(|task| {
                    task.state != deck_runtime::State::Destroyed
                        && task.spec.project.as_deref() == Some(project.id.as_str())
                })
                .count(),
        })
        .collect();
    let rows = sidebar::rows(&needs, &projects, d.project(), d.rail().collapsed(), d.settled().len());
    rows.iter()
        .map(|row| match row {
            sidebar::Row::NeedsHeader { count } => json!({ "kind": "needs_header", "count": count }),
            sidebar::Row::Need { index, project_name, title, word, .. } => json!({
                "kind": "need", "index": index, "project": project_name, "title": title, "word": word,
            }),
            sidebar::Row::Nothing => json!({ "kind": "nothing" }),
            sidebar::Row::ProjectsHeader => json!({ "kind": "projects_header" }),
            sidebar::Row::Project { index, id, name, tasks, attention, selected, open } => json!({
                "kind": "project", "index": index, "id": id, "name": name,
                "tasks": tasks, "attention": attention, "selected": selected, "open": open,
            }),
            sidebar::Row::Tasks => json!({ "kind": "tasks" }),
            sidebar::Row::Settled { count } => json!({ "kind": "settled", "count": count }),
        })
        .collect()
}

/// The settings panel as the control socket reports it (ADR 0012 §4): what
/// is drawn, never a field's text, so a secret being typed cannot leave the
/// window through `state`.
fn settings_state(d: &Deck) -> serde_json::Value {
    use crate::ui::settings::{Confirm, Form, Key};
    let s = d.settings();
    json!({
        "open": s.is_open(),
        "revision": s.snapshot().map(|snap| snap.revision),
        "stored": s.snapshot().map(|snap| snap.stored),
        "overrides": s.snapshot().map(|snap| snap.overrides.clone()).unwrap_or_default(),
        "default": s.settings().map(|settings| settings.default.clone()),
        "providers": s.provider_rows().iter().map(|row| json!({
            "name": row.name,
            "url": row.url,
            "secret": row.secret,
            "key": match &row.key {
                Key::Set(_) => "set",
                Key::Revoked(_) => "revoked",
                Key::NotSet => "not_set",
                Key::Keyless => "keyless",
                Key::Unknown => "unknown",
            },
            "models": row.models,
            "default": row.default,
        })).collect::<Vec<_>>(),
        "secrets": s.secret_rows().iter().map(|row| json!({
            "name": row.name,
            "fingerprint": row.listed.as_ref().and_then(|l| l.fingerprint.clone()),
            "providers": row.providers,
        })).collect::<Vec<_>>(),
        "form": s.form().map(|form| match form {
            Form::Provider { editing, .. } => json!({ "kind": "provider", "editing": editing }),
            Form::Secret { name, .. } => json!({ "kind": "secret", "name": name }),
        }),
        "focused": s.focused().map(|f| format!("{f:?}").to_lowercase()),
        "confirm": s.confirm().map(|c| match c {
            Confirm::DeleteProvider(name) => json!({ "delete_provider": name }),
            Confirm::DeleteSecret(name) => json!({ "delete_secret": name }),
        }),
        "busy": s.busy(),
        "error": s.error(),
        "notice": s.notice(),
        "models": s.discovery().map(|d| json!({
            "provider": d.provider,
            "loading": d.loading,
            "count": d.models.len(),
            "shown": s.discovery_shown().map(|(rows, _)| rows),
            "query": d.query.text(),
            "stale": d.stale,
            "error": d.error,
        })),
        "session_model": s.session_model(),
        "picking": s.picking(),
    })
}
