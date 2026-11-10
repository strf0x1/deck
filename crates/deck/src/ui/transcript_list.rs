//! The transcript pane as a virtualized list: rows from `transcript.rs`
//! drawn by `cards.rs`, following new output until the user scrolls up,
//! with a pill back to the end.
//!
//! The design choice: GPUI's `list` measures only rows near the viewport,
//! so a long session costs what is on screen. The Deck tells it which rows
//! changed by comparing row signatures every render, from the first change
//! to the end, so streaming re-measures the last row alone.

use super::panels::glyph;
use super::root::Deck;
use super::session::Status;
use super::theme::{Tokens, icon, size, space};
use gpui::{
    AnyElement, Context, FollowMode, FontWeight, InteractiveElement as _, IntoElement, ListAlignment, ListState,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, list, px,
};

/// A transcript list that sticks to new output until the user scrolls up.
pub(super) fn following() -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(600.));
    list.set_follow_mode(FollowMode::Tail);
    list
}

impl Deck {
    /// Open the card that draws entry `entry` and scroll it into view.
    pub(super) fn reveal(&mut self, entry: usize, cx: &mut Context<Self>) {
        let rows = super::transcript::rows(&self.session().entries, self.session().idle());
        let Some(ix) = super::transcript::row_of(&rows, entry) else { return };
        let key = rows[ix].key(&self.session().entries);
        if self.expanded.insert(key.clone()) {
            self.motion.open(&key);
        }
        // Measure the opened card before scrolling to it.
        self.sync_transcript();
        self.transcript.scroll_to_reveal_item(ix);
        cx.notify();
    }

    /// Move the keyboard selection to the previous (`up`) or next card and
    /// scroll it into view.
    pub(super) fn select_card(&mut self, up: bool, cx: &mut Context<Self>) {
        let entries = &self.session().entries;
        let rows = super::transcript::rows(entries, self.session().idle());
        let current = self.focused.as_ref().and_then(|k| rows.iter().position(|r| r.key(entries) == *k));
        let Some(ix) = super::transcript::step(&super::transcript::expandable(entries, &rows), current, up) else {
            return;
        };
        self.focused = Some(rows[ix].key(entries));
        self.transcript.scroll_to_reveal_item(ix);
        cx.notify();
    }

    /// Open or fold the selected card; `false` when none is selected.
    pub(super) fn toggle_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(key) = self.focused.clone() else { return false };
        self.toggle(&key, cx);
        true
    }

    /// The key of the keyboard-selected row.
    pub fn selected_card(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Expand or fold a transcript row.
    pub(super) fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
            self.motion.open(key);
        }
        cx.notify();
    }

    /// Tell the list which rows changed since it last measured: from the
    /// first changed signature to the end. Run every render, so no session
    /// change can be missed; while streaming it is the last row alone.
    pub(super) fn sync_transcript(&mut self) {
        let entries = &self.session().entries;
        let rows = super::transcript::rows(entries, self.session().idle());
        let sigs = super::transcript::signatures(entries, &rows, &self.expanded);
        let first = super::transcript::first_change(&self.row_sigs, &sigs);
        if first < self.row_sigs.len() || first < sigs.len() {
            self.transcript.splice(first..self.row_sigs.len(), sigs.len() - first);
        }
        self.row_sigs = sigs;
    }

    /// Whether the transcript row under `key` (`transcript::Row::key`) is expanded.
    pub fn is_expanded(&self, key: &str) -> bool {
        self.expanded.contains(key)
    }

    /// Whether the transcript has stopped following new output.
    pub fn scrolled_back(&self) -> bool {
        !self.transcript.is_following_tail() && !self.row_sigs.is_empty()
    }
}

/// The transcript: a virtualized list of rows (`cards.rs`) that follows new
/// output, with a pill back to the end once the user scrolls up.
pub fn transcript(deck: &Deck, cx: &mut Context<Deck>) -> AnyElement {
    if deck.session().entries.is_empty() && deck.session().status != Status::Detached {
        return div()
            .debug_selector(|| "transcript-empty".into())
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(space::TIGHT))
            .child(
                div().text_size(px(size::DISPLAY)).font_weight(FontWeight::SEMIBOLD).child("What should we work on?"),
            )
            .child(
                div().text_color(Tokens::text_faint()).child("The agent's work, tool calls and questions land here."),
            )
            .into_any_element();
    }
    let rows = list(
        deck.transcript.clone(),
        cx.processor(|this, ix: usize, _, cx| {
            let rows = super::transcript::rows(&this.session().entries, this.session().idle());
            let Some(row) = rows.get(ix) else { return div().into_any_element() };
            div()
                .group(super::copy::ROW)
                .px(px(space::MARGIN))
                .pt(px(if ix == 0 { 18. } else { space::ROW }))
                .child(super::cards::row(this, row, cx))
                .into_any_element()
        }),
    )
    .size_full()
    .pb(px(space::ROW));
    // The pill rises 16px and fades in as it appears, and drops as it goes.
    let shown = deck.motion.pill();
    let jump = (shown > 0.01).then(|| {
        div().absolute().bottom(px(12. - 16. * (1. - shown))).opacity(shown).w_full().flex().justify_center().child(
            div()
                .id("jump-latest")
                .debug_selector(|| "jump-latest".into())
                .h(px(28.))
                .px(px(space::INSET))
                .flex()
                .items_center()
                .gap(px(7.))
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge_strong())
                .bg(Tokens::float())
                .text_size(px(size::SMALL))
                .text_color(Tokens::text_dim())
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.transcript.set_follow_mode(FollowMode::Tail);
                    cx.notify();
                }))
                .child(glyph(icon::DOWN, 10.))
                .child("Jump to latest"),
        )
    });
    div()
        .id("transcript")
        .debug_selector(|| "transcript".into())
        .relative()
        .flex_1()
        .min_h_0()
        .child(rows)
        .children(jump)
        .into_any_element()
}
