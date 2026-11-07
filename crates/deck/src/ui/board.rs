//! The lane board: a column of agent cards the user reorders by dragging.
//! The dragged card follows the pointer, the others slide out of its way,
//! and on release it springs into its slot.
//!
//! The design choice: deck draws its own drag instead of GPUI's drag-and-drop
//! (`on_drag`), which renders a detached preview. Here the card itself moves,
//! and every card's position is a [`Spring`], so displacement and settling
//! are the same motion. The board is plain state; the root view routes mouse
//! events to it and asks for frames while it is moving.

use super::spring::Spring;

pub const CARD_H: f32 = 64.;
pub const GAP: f32 = 8.;
pub const PITCH: f32 = CARD_H + GAP;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneState {
    #[default]
    Idle,
    Working,
    Waiting,
    Review,
}

#[derive(Debug, Clone, Default)]
pub struct Lane {
    pub id: usize,
    pub title: String,
    pub detail: String,
    pub state: LaneState,
    /// The task's publication chip: its state dot and `#7` or branch.
    pub chip: Option<(super::publication::Dot, String)>,
}

pub struct Card {
    pub lane: Lane,
    pub y: Spring,
    /// 0 at rest, 1 lifted: drives the sideways shift and edge brightness.
    pub lift: Spring,
}

struct Drag {
    id: usize,
    /// Pointer y minus the card's y at grab time.
    grab: f32,
}

pub struct Board {
    /// In slot order: `cards[i]` belongs in slot `i`.
    cards: Vec<Card>,
    drag: Option<Drag>,
}

/// The slot a card whose top is at `y` should occupy among `n`.
pub fn slot_for(y: f32, n: usize) -> usize {
    let raw = (y / PITCH).round();
    if raw <= 0. { 0 } else { (raw as usize).min(n.saturating_sub(1)) }
}

impl Board {
    pub fn new(lanes: Vec<Lane>) -> Self {
        let cards = lanes
            .into_iter()
            .enumerate()
            .map(|(i, lane)| Card { lane, y: Spring::at(i as f32 * PITCH), lift: Spring::at(0.) })
            .collect();
        Board { cards, drag: None }
    }

    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    pub fn order(&self) -> Vec<usize> {
        self.cards.iter().map(|c| c.lane.id).collect()
    }

    pub fn dragging(&self) -> Option<usize> {
        self.drag.as_ref().map(|d| d.id)
    }

    pub fn height(&self) -> f32 {
        self.cards.len() as f32 * PITCH - GAP
    }

    /// Show exactly `lanes`: cards that stay keep their slot order and
    /// motion, new ones join at the bottom, and missing ones leave. A card
    /// under the pointer that leaves ends the drag.
    pub fn sync(&mut self, mut lanes: Vec<Lane>) {
        let mut cards = Vec::with_capacity(lanes.len());
        for card in self.cards.drain(..) {
            if let Some(i) = lanes.iter().position(|l| l.id == card.lane.id) {
                cards.push(Card { lane: lanes.remove(i), ..card });
            }
        }
        let start = cards.len();
        cards.extend(lanes.into_iter().enumerate().map(|(i, lane)| Card {
            lane,
            y: Spring::at((start + i) as f32 * PITCH),
            lift: Spring::at(0.),
        }));
        self.cards = cards;
        if self.drag.as_ref().is_some_and(|d| !self.cards.iter().any(|c| c.lane.id == d.id)) {
            self.drag = None;
        }
        self.retarget();
    }

    pub fn lane_mut(&mut self, id: usize) -> Option<&mut Lane> {
        self.cards.iter_mut().map(|c| &mut c.lane).find(|l| l.id == id)
    }

    /// Pick up card `id`; `pointer_y` is in any space, as long as moves use the same one.
    pub fn grab(&mut self, id: usize, pointer_y: f32) {
        let Some(card) = self.cards.iter_mut().find(|c| c.lane.id == id) else { return };
        card.lift.target = 1.;
        self.drag = Some(Drag { id, grab: pointer_y - card.y.pos });
    }

    pub fn move_to(&mut self, pointer_y: f32) {
        let Some(drag) = &self.drag else { return };
        let Some(from) = self.cards.iter().position(|c| c.lane.id == drag.id) else { return };
        let y = (pointer_y - drag.grab).clamp(-PITCH * 0.5, self.height() - CARD_H + PITCH * 0.5);
        self.cards[from].y.set(y);
        let to = slot_for(y, self.cards.len());
        if to != from {
            let card = self.cards.remove(from);
            self.cards.insert(to, card);
        }
        self.retarget();
    }

    pub fn release(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        if let Some(card) = self.cards.iter_mut().find(|c| c.lane.id == drag.id) {
            card.lift.target = 0.;
        }
        self.retarget();
    }

    /// Every card not under the pointer chases its slot.
    fn retarget(&mut self) {
        let held = self.dragging();
        for (i, card) in self.cards.iter_mut().enumerate() {
            if Some(card.lane.id) != held {
                card.y.target = i as f32 * PITCH;
            }
        }
    }

    /// Advance motion by `dt` seconds; true while anything still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        let held = self.dragging();
        let mut moving = held.is_some();
        for card in &mut self.cards {
            if Some(card.lane.id) != held {
                card.y.step(dt);
            }
            card.lift.step(dt);
            moving |= !card.y.at_rest() || !card.lift.at_rest();
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lanes(n: usize) -> Vec<Lane> {
        (0..n).map(|id| Lane { id, title: format!("l{id}"), ..Lane::default() }).collect()
    }

    fn settle(b: &mut Board) {
        for _ in 0..600 {
            if !b.step(1. / 60.) {
                return;
            }
        }
        panic!("board never settled");
    }

    #[test]
    fn slots_round_to_the_nearest_and_clamp() {
        assert_eq!(slot_for(-40., 4), 0);
        assert_eq!(slot_for(PITCH * 0.49, 4), 0);
        assert_eq!(slot_for(PITCH * 0.51, 4), 1);
        assert_eq!(slot_for(PITCH * 9., 4), 3);
    }

    #[test]
    fn dragging_the_first_card_down_two_slots_reorders_and_settles() {
        let mut b = Board::new(lanes(4));
        b.grab(0, 10.);
        b.move_to(10. + PITCH * 2.1);
        assert_eq!(b.order(), vec![1, 2, 0, 3]);
        // Displaced cards are still travelling up toward their new slots.
        assert!(b.step(1. / 60.));
        b.release();
        settle(&mut b);
        let ys: Vec<f32> = b.cards().iter().map(|c| c.y.pos).collect();
        assert_eq!(ys, vec![0., PITCH, PITCH * 2., PITCH * 3.]);
        assert!(b.cards().iter().all(|c| c.lift.pos == 0.));
    }

    #[test]
    fn the_held_card_tracks_the_pointer_exactly() {
        let mut b = Board::new(lanes(3));
        b.grab(1, PITCH + 5.);
        b.move_to(PITCH + 25.);
        let held = b.cards().iter().find(|c| c.lane.id == 1).map(|c| c.y.pos);
        assert_eq!(held, Some(PITCH + 20.));
    }

    #[test]
    fn syncing_keeps_order_and_motion_adds_at_the_bottom_and_drops_the_missing() {
        let mut b = Board::new(lanes(3));
        b.grab(0, 0.);
        b.move_to(PITCH * 2.);
        b.release();
        settle(&mut b);
        assert_eq!(b.order(), vec![1, 2, 0]);
        let mut next = lanes(4);
        next.remove(2);
        next[0].title = "renamed".into();
        b.sync(next);
        assert_eq!(b.order(), vec![1, 0, 3], "kept cards keep their order; 2 left; 3 joined");
        assert_eq!(b.cards()[1].lane.title, "renamed");
        settle(&mut b);
        let ys: Vec<f32> = b.cards().iter().map(|c| c.y.pos).collect();
        assert_eq!(ys, vec![0., PITCH, PITCH * 2.]);
        b.grab(3, PITCH * 2.);
        b.sync(lanes(1));
        assert_eq!(b.dragging(), None, "a card that left cannot stay held");
    }
}
