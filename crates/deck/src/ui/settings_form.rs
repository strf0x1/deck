//! The settings panel's forms and marks: a provider's fields, a secret's
//! masked field, the delete confirmation, banners and status dots. No state
//! of its own.
//!
//! The design choice: only the focused field is the live text element
//! ([`composer::Field`](super::composer::Field), masked for a secret); the
//! others are plain text that a click focuses. So one input handler is ever
//! registered, and it always belongs to the field the caret is in.

use super::access_panel::button;
use super::composer;
use super::panels::{eyebrow, glyph};
use super::root::Deck;
use super::settings::{Confirm, Field, Form};
use super::theme::{MONO, Tokens, icon, radius, size, space, tint};
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The provider form: name, url, key name, models.
pub fn provider_form(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(Form::Provider { editing, name, url, key, models, focus, .. }) = deck.settings().form() else {
        return div().into_any_element();
    };
    let title = match editing {
        Some(old) => format!("Edit {old}"),
        None => "Add a provider".into(),
    };
    let fields = [
        (Field::Name, "Name", name.text(), "lowercase, e.g. openrouter or local"),
        (Field::Url, "URL", url.text(), "https://openrouter.ai/api/v1"),
        (Field::Key, "Key name", key.text(), "empty: <NAME>_API_KEY · none: keyless server"),
        (Field::Models, "Models", models.text(), "one per line"),
    ];
    let rows = fields
        .into_iter()
        .map(|(field, label, text, hint)| field_row(field, label, text, hint, *focus == field, false, cx))
        .collect::<Vec<_>>();
    let mut rows = rows;
    // A saved provider can name itself to `deckd`, so only its form offers
    // the fetched list; a new one is saved first.
    if let Some(saved) = editing
        .as_deref()
        .and_then(|name| deck.settings().settings().and_then(|s| s.provider(name)).map(|p| p.url.clone()))
    {
        rows.push(discovery_block(deck, cx, url.text(), &saved));
    }
    form_card(title, rows, "Enter saves · Tab moves · in Models, ctrl-Enter saves", deck.settings().busy(), cx)
}

/// The provider's models as its endpoint serves them: fetch, filter, and a
/// click to add or remove one from the Models allowlist (ADR 0012 §4).
fn discovery_block(deck: &Deck, cx: &mut Context<Deck>, url: &str, saved: &str) -> AnyElement {
    let Some(Form::Provider { models, focus, discovery, .. }) = deck.settings().form() else {
        return div().into_any_element();
    };
    let Some(discovery) = discovery else { return fetch_row(cx, url, saved) };
    let listed = models.text().to_owned();
    let focused = *focus == Field::Search;
    let query = discovery.query.text().to_owned();
    let (rows, total) = super::model_search::shown(&discovery.models, &query);
    let drawn = rows.len();
    let model_rows = rows
        .into_iter()
        .enumerate()
        .map(|(i, model)| {
            let on = super::settings_fields::lists(&listed, &model.id);
            let id = model.id.clone();
            let context = model.context.map(|tokens| format!("{}k", tokens / 1000));
            div()
                .id(("settings-model", i))
                .debug_selector(move || format!("settings-model-{i}"))
                .min_h(px(26.))
                .px(px(space::TIGHT))
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .rounded(px(radius::CONTROL))
                .cursor_pointer()
                .hover(|style| style.bg(Tokens::raised_hover()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_model(&id, cx)))
                .child(div().w(px(12.)).children(on.then(|| glyph(icon::CHECK, 9.).text_color(Tokens::lime()))))
                .child(
                    div()
                        .debug_selector(move || format!("settings-model-label-{i}"))
                        .font_family(MONO)
                        .text_size(size::scaled(size::SMALL))
                        .child(model.id.clone()),
                )
                .children((model.name != model.id && !model.name.is_empty()).then(|| {
                    div()
                        .text_size(size::scaled(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child(model.name.clone())
                }))
                .child(div().flex_1())
                .children(context.map(|tokens| {
                    div()
                        .font_family(MONO)
                        .text_size(size::scaled(size::MICRO))
                        .text_color(Tokens::text_faint())
                        .child(tokens)
                }))
        })
        .collect::<Vec<_>>();
    let note = discovery.error.as_ref().map(|error| {
        let (color, text) = if discovery.stale {
            (Tokens::amber(), format!("Using the cached list: {error}"))
        } else {
            (Tokens::red(), error.clone())
        };
        div()
            .debug_selector(|| "settings-models-error".into())
            .text_size(size::scaled(size::SMALL))
            .text_color(color)
            .child(text)
    });
    let search: Option<AnyElement> = (!discovery.models.is_empty()).then(|| {
        let body: AnyElement = if focused {
            composer::Field::new(cx.entity(), Tokens::accent(), "search models…").into_any_element()
        } else if query.is_empty() {
            div().text_color(Tokens::text_faint()).child("search models…").into_any_element()
        } else {
            div().child(query.clone()).into_any_element()
        };
        div()
            .id("settings-model-search")
            .debug_selector(|| "settings-model-search".into())
            .flex()
            .items_center()
            .gap(px(space::TIGHT))
            .px(px(space::INSET))
            .py(px(3.))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(if focused { tint(Tokens::accent(), 0.55) } else { Tokens::edge().into() })
            .bg(Tokens::raised())
            .font_family(MONO)
            .text_size(size::scaled(size::SMALL))
            .cursor_text()
            .on_click(cx.listener(|this, _, _, cx| this.focus_settings_field(Field::Search, cx)))
            .child(glyph(icon::SEARCH, 10.).text_color(Tokens::text_faint()))
            .child(body)
            .into_any_element()
    });
    let footer = if discovery.models.is_empty() {
        (discovery.loading).then_some("Fetching the provider's models…")
    } else if total == 0 {
        Some("No model matches.")
    } else if total > drawn {
        Some("Showing the first matches; type to narrow.")
    } else {
        Some("Click a model to add or remove it; Enter adds the first match.")
    };
    div()
        .debug_selector(|| "settings-models".into())
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .p(px(space::INSET))
        .rounded(px(radius::CARD))
        .border_1()
        .border_color(Tokens::edge())
        .bg(Tokens::sunken())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .child(eyebrow("Models from the provider"))
                .child(div().flex_1())
                .child(
                    button("settings-refresh-models", "Refresh", Tokens::text_dim())
                        .debug_selector(|| "settings-refresh-models".into())
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_models(cx))),
                )
                .child(
                    super::chrome::icon_button("settings-models-close", icon::CLOSE)
                        .debug_selector(|| "settings-models-close".into())
                        .on_click(cx.listener(|this, _, _, cx| this.close_models(cx))),
                ),
        )
        .children(note)
        .children(search)
        .child(
            div().id("settings-models-list").flex().flex_col().max_h(px(260.)).overflow_y_scroll().children(model_rows),
        )
        .children(footer.map(|text| {
            div().text_size(size::scaled(size::SMALL)).text_color(Tokens::text_faint()).child(text.to_owned())
        }))
        .into_any_element()
}

fn fetch_row(cx: &mut Context<Deck>, url: &str, saved: &str) -> AnyElement {
    if saved != url.trim() {
        return div()
            .debug_selector(|| "settings-fetch-unsaved".into())
            .text_size(size::scaled(size::SMALL))
            .text_color(Tokens::text_faint())
            .child("Save the new URL first, then fetch its models.")
            .into_any_element();
    }
    div()
        .flex()
        .items_center()
        .gap(px(space::STACK))
        .child(
            button("settings-fetch-models", "Fetch models", Tokens::text_dim())
                .debug_selector(|| "settings-fetch-models".into())
                .on_click(cx.listener(|this, _, _, cx| this.fetch_models(cx))),
        )
        .child(
            div()
                .text_size(size::scaled(size::SMALL))
                .text_color(Tokens::text_faint())
                .child(format!("Ask {url} for the models it serves, then click one to add it.")),
        )
        .into_any_element()
}

/// The masked secret form, under the secret's row.
pub fn secret_form(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    let Some(Form::Secret { name, .. }) = deck.settings().form() else { return div().into_any_element() };
    let rows = vec![field_row(Field::Secret, "Value", "", "paste the key", true, true, cx)];
    form_card(
        format!("Set {name}"),
        rows,
        "Stored by deckd; the window keeps nothing. Enter saves, Escape cancels.",
        deck.settings().busy(),
        cx,
    )
}

fn form_card(title: String, rows: Vec<AnyElement>, hint: &str, busy: bool, cx: &mut Context<Deck>) -> AnyElement {
    div()
        .debug_selector(|| "settings-form".into())
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::accent(), 0.3))
        .bg(tint(Tokens::accent(), 0.04))
        .child(div().font_weight(FontWeight::MEDIUM).child(title))
        .children(rows)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(space::LINE))
                .child(
                    button("settings-save", if busy { "Saving…" } else { "Save" }, Tokens::accent())
                        .debug_selector(|| "settings-save".into())
                        .on_click(cx.listener(|this, _, _, cx| this.submit_settings_form(cx))),
                )
                .child(
                    button("settings-cancel", "Cancel", Tokens::text_dim())
                        .debug_selector(|| "settings-cancel".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings_cancel(cx);
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(size::scaled(size::SMALL))
                        .text_color(Tokens::text_faint())
                        .child(hint.to_owned()),
                ),
        )
        .into_any_element()
}

fn field_row(
    field: Field,
    label: &'static str,
    text: &str,
    hint: &'static str,
    focused: bool,
    masked: bool,
    cx: &mut Context<Deck>,
) -> AnyElement {
    let selector = format!("settings-field-{}", field_name(field));
    let body: AnyElement = if focused {
        composer::Field::new(cx.entity(), Tokens::accent(), hint).masked(masked).into_any_element()
    } else if text.is_empty() {
        div().text_color(Tokens::text_faint()).child(hint).into_any_element()
    } else {
        div().whitespace_normal().child(text.to_owned()).into_any_element()
    };
    div()
        .flex()
        .gap(px(space::STACK))
        .child(
            div()
                .flex_none()
                .w(px(78.))
                .pt(px(5.))
                .text_size(size::scaled(size::LABEL))
                .text_color(Tokens::text_dim())
                .child(label),
        )
        .child(
            div()
                .id(gpui::SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .flex_1()
                .min_w_0()
                .px(px(space::INSET))
                .py(px(4.))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_color(if focused { tint(Tokens::accent(), 0.55) } else { Tokens::edge().into() })
                .bg(Tokens::sunken())
                .font_family(MONO)
                .text_size(size::scaled(size::LABEL))
                .cursor_text()
                .on_click(cx.listener(move |this, _, _, cx| this.focus_settings_field(field, cx)))
                .child(body),
        )
        .into_any_element()
}

fn field_name(field: Field) -> &'static str {
    match field {
        Field::Name => "name",
        Field::Url => "url",
        Field::Key => "key",
        Field::Models => "models",
        Field::Search => "search",
        Field::Secret => "secret",
    }
}

pub(super) fn status(color: Hsla, label: String) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(space::TIGHT))
        .text_size(size::scaled(size::SMALL))
        .text_color(color)
        .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
        // Clipped, not truncated: a truncating text in a flex row measures as
        // its ellipsis and collapses to "…" with room to spare (`prompt.rs`).
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().child(label))
}

pub(super) fn tag(label: &'static str, color: Hsla) -> gpui::Div {
    div()
        .px(px(6.))
        .rounded_full()
        .bg(tint(color, 0.12))
        .text_color(color)
        .text_size(size::scaled(size::MICRO))
        .font_family(MONO)
        .child(label)
}

pub(super) fn banner(g: &'static str, color: Hsla, text: String) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .gap(px(space::LINE))
        .px(px(space::INSET))
        .py(px(7.))
        .rounded(px(radius::CONTROL))
        .border_1()
        .border_color(tint(color, 0.25))
        .bg(tint(color, 0.06))
        .text_size(size::scaled(size::SMALL))
        .text_color(Tokens::text_dim())
        .child(glyph(g, 10.).text_color(color))
        .child(div().flex_1().min_w_0().child(text))
        .into_any_element()
}

pub(super) fn confirm(target: &Confirm, cx: &mut Context<Deck>) -> AnyElement {
    let (title, detail, name, provider) = match target {
        Confirm::DeleteProvider(name) => (
            format!("Delete provider {name}?"),
            "Running tasks keep the provider they were created with. New sessions cannot use it.".to_string(),
            name.clone(),
            true,
        ),
        Confirm::DeleteSecret(name) => (
            format!("Delete {name}?"),
            "Tasks that use it fail their next model request until a value is set again. \
             It is not revoked at the provider."
                .to_string(),
            name.clone(),
            false,
        ),
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::STACK))
        .p(px(space::PAD))
        .rounded(px(radius::BUBBLE))
        .border_1()
        .border_color(tint(Tokens::red(), 0.35))
        .bg(tint(Tokens::red(), 0.06))
        .child(div().font_weight(FontWeight::MEDIUM).child(title))
        .child(div().text_size(size::scaled(size::LABEL)).text_color(Tokens::text_dim()).child(detail))
        .child(
            div()
                .flex()
                .gap(px(space::LINE))
                .child(
                    button("settings-confirm", "Delete", Tokens::red())
                        .debug_selector(|| "settings-confirm".into())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if provider { this.delete_provider(&name, cx) } else { this.delete_secret(&name, cx) }
                        })),
                )
                .child(
                    button("settings-confirm-cancel", "Cancel", Tokens::text_dim())
                        .on_click(cx.listener(|this, _, _, cx| this.settings_cancel(cx))),
                ),
        )
        .into_any_element()
}
