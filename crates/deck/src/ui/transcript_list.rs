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
use super::theme::{Tokens, icon};
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
    /// Expand or fold a transcript row.
    pub(super) fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        cx.notify();
    }

    /// Tell the list which rows changed since it last measured: from the
    /// first changed signature to the end. Run every render, so no session
    /// change can be missed; while streaming it is the last row alone.
    pub(super) fn sync_transcript(&mut self) {
        let entries = &self.session.entries;
        let rows = super::transcript::rows(entries);
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
    if deck.session.entries.is_empty() && deck.session.status != Status::Detached {
        return div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("What should we work on?"))
            .child(
                div().text_color(Tokens::text_faint()).child("The agent's work, tool calls and questions land here."),
            )
            .into_any_element();
    }
    let rows = list(
        deck.transcript.clone(),
        cx.processor(|this, ix: usize, _, cx| {
            let rows = super::transcript::rows(&this.session.entries);
            let Some(row) = rows.get(ix) else { return div().into_any_element() };
            // Rows sit 14px apart, inside the pane's 22px side margins.
            div()
                .px(px(22.))
                .pt(px(if ix == 0 { 18. } else { 14. }))
                .child(super::cards::row(this, row, cx))
                .into_any_element()
        }),
    )
    .size_full()
    .pb(px(14.));
    let jump = deck.scrolled_back().then(|| {
        div().absolute().bottom(px(12.)).w_full().flex().justify_center().child(
            div()
                .id("jump-latest")
                .debug_selector(|| "jump-latest".into())
                .h(px(28.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(7.))
                .rounded_full()
                .border_1()
                .border_color(Tokens::edge_strong())
                .bg(Tokens::float())
                .text_size(px(11.5))
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
