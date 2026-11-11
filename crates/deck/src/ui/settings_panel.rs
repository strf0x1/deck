//! Renders the settings panel from [`SettingsPanel`](super::settings::SettingsPanel)
//! (ADR 0012 §4): providers, the default model, and the stored secrets. It
//! takes the conversation pane's body while open. No state of its own.
//!
//! The design choice: colour by what each mark means. A key that is set is
//! green, one a provider needs and does not have is amber (waiting on the
//! human), and red appears only on delete; the environment's overrides are
//! amber too, because they are why the panel and a new task may disagree.

use super::access_panel::{button, error};
use super::panels::{eyebrow, glyph, icon_tile};
use super::root::Deck;
use super::settings::{Form, Key};
use super::settings_form::{banner, confirm, provider_form, secret_form, status, tag};
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

pub fn panel(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let s = deck.settings();
    let notes = s.snapshot().map(|snap| {
        let mut notes: Vec<String> =
            snap.overrides.iter().map(|o| format!("{o}: new tasks use the variable.")).collect();
        if !snap.stored {
            notes.push("deckd is using its built-in provider. Saving writes settings.json.".into());
        }
        notes
    });
    let body = div()
        .id("settings-body")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(space::ROW))
        .child(providers(deck, cx))
        .child(default_model(deck, cx))
        .child(secrets(deck, cx))
        .child(
            div()
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_faint())
                .child("Settings apply to VM tasks, each pinned at create. A local task uses Construct's own config."),
        );
    div()
        .id("settings")
        .debug_selector(|| "settings-panel".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .children(notes.into_iter().flatten().map(|note| banner(icon::WARN, Tokens::amber(), note)))
        .children(s.confirm().map(|c| confirm(c, cx)))
        .children(s.error().map(error))
        .children(s.notice().map(|n| banner(icon::CHECK, Tokens::lime(), n.to_owned())))
        .child(if s.snapshot().is_none() && s.error().is_none() {
            div().text_color(Tokens::text_faint()).child("Loading settings…").into_any_element()
        } else {
            body.into_any_element()
        })
        .into_any_element()
}

fn providers(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let s = deck.settings();
    let editing = match s.form() {
        Some(Form::Provider { editing, .. }) => Some(editing.clone()),
        _ => None,
    };
    let rows = s.provider_rows().into_iter().enumerate().map(|(i, row)| {
        if editing.as_ref() == Some(&Some(row.name.clone())) {
            return provider_form(deck, cx);
        }
        let (label, color): (String, Hsla) = match &row.key {
            Key::Set(f) => (format!("key set · {f}"), Tokens::lime()),
            Key::Revoked(f) => (format!("key revoked · {f}"), Tokens::red()),
            Key::NotSet => (format!("{} not set", row.secret.as_deref().unwrap_or("key")), Tokens::amber()),
            Key::Keyless => ("keyless".into(), Tokens::text_dim().into()),
            Key::Unknown => (row.secret.clone().unwrap_or_default(), Tokens::text_faint().into()),
        };
        let (edit, delete) = (row.name.clone(), row.name.clone());
        let set_key = (row.key == Key::NotSet).then(|| row.secret.clone()).flatten();
        div()
            .debug_selector(move || format!("settings-provider-{i}"))
            .flex()
            .items_center()
            .gap(px(space::STACK))
            .px(px(space::INSET))
            .py(px(9.))
            .rounded(px(radius::CARD))
            .border_1()
            .border_color(Tokens::edge())
            .bg(Tokens::raised())
            .child(icon_tile(icon::SERVER, Tokens::hue(&row.name), 24.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(space::LINE))
                            .child(div().font_family(MONO).font_weight(FontWeight::MEDIUM).child(row.name.clone()))
                            .children(row.default.then(|| tag("default", Tokens::accent())))
                            .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(format!(
                                "{} model{}",
                                row.models,
                                if row.models == 1 { "" } else { "s" }
                            ))),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(size::SMALL))
                            .text_color(Tokens::text_faint())
                            .child(row.url.clone()),
                    )
                    .child(status(color, label)),
            )
            .children(set_key.map(|name| {
                button(("settings-provider-key", i), "Set key", Tokens::amber())
                    .on_click(cx.listener(move |this, _, _, cx| this.open_secret(&name, cx)))
            }))
            .child(
                button(("settings-provider-edit", i), "Edit", Tokens::text_dim())
                    .debug_selector(move || format!("settings-provider-edit-{i}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.edit_provider(&edit, cx))),
            )
            .child(
                button(("settings-provider-delete", i), "Delete", Tokens::red())
                    .debug_selector(move || format!("settings-provider-delete-{i}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.delete_provider(&delete, cx))),
            )
            .into_any_element()
    });
    let rows: Vec<AnyElement> = rows.collect();
    section(
        "Providers",
        Some(
            button("settings-add-provider", "Add provider", Tokens::accent())
                .debug_selector(|| "settings-add-provider".into())
                .on_click(cx.listener(|this, _, _, cx| this.add_provider(cx)))
                .into_any_element(),
        ),
    )
    .children((editing == Some(None)).then(|| provider_form(deck, cx)))
    .children(rows)
    .into_any_element()
}

fn default_model(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let s = deck.settings();
    let current = s.settings().map(|settings| settings.default.clone());
    let chips = s.choices().into_iter().enumerate().map(|(i, choice)| {
        let on = current.as_deref() == Some(choice.as_str());
        let fg: Hsla = if on { Tokens::accent() } else { Tokens::text_dim().into() };
        let pick = choice.clone();
        div()
            .id(("settings-default", i))
            .debug_selector(move || format!("settings-default-{i}"))
            .h(px(26.))
            .px(px(space::INSET))
            .flex()
            .items_center()
            .gap(px(space::TIGHT))
            .rounded_full()
            .border_1()
            .border_color(tint(fg, if on { 0.5 } else { 0.2 }))
            .bg(tint(fg, if on { 0.12 } else { 0.03 }))
            .text_color(fg)
            .font_family(MONO)
            .text_size(px(size::SMALL))
            .cursor_pointer()
            .hover(move |st| st.bg(tint(fg, 0.12)))
            .on_click(cx.listener(move |this, _, _, cx| this.set_default_model(&pick, cx)))
            .children(on.then(|| glyph(icon::CHECK, 9.)))
            .child(choice)
    });
    section("Default model", None)
        .child(
            div()
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_faint())
                .child("New sessions start here unless you pick another beside New session."),
        )
        .child(div().flex().flex_wrap().gap(px(space::LINE)).children(chips))
        .into_any_element()
}

fn secrets(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let s = deck.settings();
    let open = match s.form() {
        Some(Form::Secret { name, .. }) => Some(name.clone()),
        _ => None,
    };
    let rows = s.secret_rows().into_iter().enumerate().map(|(i, row)| {
        let listed = row.listed.as_ref();
        let fingerprint = listed.and_then(|l| l.fingerprint.clone());
        let (label, color): (String, Hsla) = match (listed, &fingerprint) {
            (Some(l), Some(f)) if l.revoked => (format!("revoked · {f}"), Tokens::red()),
            (_, Some(f)) => (format!("set · {f}"), Tokens::lime()),
            (Some(l), None) if l.error.as_deref() != Some("no value") => {
                (l.error.clone().unwrap_or_default(), Tokens::amber())
            }
            _ if row.providers.is_empty() => ("no value".into(), Tokens::text_faint().into()),
            _ => ("not set".into(), Tokens::amber()),
        };
        let used =
            if row.providers.is_empty() { "unused".to_string() } else { format!("for {}", row.providers.join(", ")) };
        let (set, delete) = (row.name.clone(), row.name.clone());
        let card = div()
            .debug_selector(move || format!("settings-secret-{i}"))
            .flex()
            .items_center()
            .gap(px(space::STACK))
            .px(px(space::INSET))
            .py(px(9.))
            .rounded(px(radius::CARD))
            .border_1()
            .border_color(Tokens::edge())
            .bg(Tokens::raised())
            .child(glyph(icon::KEY, 11.).text_color(color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .flex()
                            .gap(px(space::LINE))
                            .child(div().font_family(MONO).text_size(px(size::LABEL)).child(row.name.clone()))
                            .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(used)),
                    )
                    .child(status(color, label)),
            )
            .child(
                button(
                    ("settings-secret-set", i),
                    if fingerprint.is_some() { "Replace" } else { "Set" },
                    Tokens::accent(),
                )
                .debug_selector(move || format!("settings-secret-set-{i}"))
                .on_click(cx.listener(move |this, _, _, cx| this.open_secret(&set, cx))),
            )
            .children(fingerprint.is_some().then(|| {
                button(("settings-secret-delete", i), "Delete", Tokens::red())
                    .debug_selector(move || format!("settings-secret-delete-{i}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.delete_secret(&delete, cx)))
            }))
            .into_any_element();
        let form = (open.as_deref() == Some(row.name.as_str())).then(|| secret_form(deck, cx));
        div().flex().flex_col().gap(px(space::LINE)).child(card).children(form).into_any_element()
    });
    let rows: Vec<AnyElement> = rows.collect();
    section("Secrets", None)
        .child(div().text_size(px(size::SMALL)).text_color(Tokens::text_faint()).child(match s.store() {
            Some(store) => format!("Kept by deckd in the {store}. The window shows names and fingerprints only."),
            None => "Kept by deckd. The window shows names and fingerprints only.".into(),
        }))
        .children(rows)
        .into_any_element()
}

fn section(title: &str, action: Option<AnyElement>) -> gpui::Div {
    div().flex().flex_col().gap(px(space::LINE)).child(
        div().flex().items_center().gap(px(space::LINE)).child(eyebrow(title)).child(div().flex_1()).children(action),
    )
}
