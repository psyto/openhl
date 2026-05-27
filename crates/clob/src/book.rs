//! Price-time priority orderbook + matching engine.
//!
//! Bids are stored with a `Reverse<Price>` key so `BTreeMap` natural-order
//! iteration walks them best-first (highest price first). Asks are stored
//! with `Price` directly so they also walk best-first (lowest price first).
//! Within each price level, orders are queued FIFO — that's the "time
//! priority" half of price-time priority.

use core::cmp::Reverse;
use std::collections::{BTreeMap, VecDeque};

use crate::types::{
    AccountId, Fill, FillResult, Order, OrderId, OrderType, Price, Qty, Side,
};

#[derive(Debug, Default)]
pub struct Book {
    /// Bids: `Reverse<Price>` key gives best-first iteration (highest first).
    bids: BTreeMap<Reverse<Price>, VecDeque<RestingOrder>>,
    /// Asks: `Price` key gives best-first iteration (lowest first).
    asks: BTreeMap<Price, VecDeque<RestingOrder>>,
}

/// An order resting on the book. Trimmed from `Order` — side and `order_type`
/// are implicit from which side of the book it's resting on, and `qty` shrinks
/// as fills consume it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RestingOrder {
    id: OrderId,
    account: AccountId,
    qty: Qty,
}

impl Book {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a taker order. Limit orders rest any unfilled remainder on the
    /// book; Market orders discard it (returned via `remaining_qty`).
    pub fn submit(&mut self, order: Order) -> FillResult {
        match order.order_type {
            OrderType::Limit { price } => self.submit_limit(order, price),
            OrderType::Market => self.submit_market(order),
        }
    }

    fn submit_limit(&mut self, order: Order, limit_price: Price) -> FillResult {
        let mut remaining = order.qty;
        let mut fills = Vec::new();

        match order.side {
            Side::Buy => {
                // Buy walks asks from cheapest; matches while ask <= limit.
                loop {
                    if remaining.0 == 0 {
                        break;
                    }
                    let Some(best_price) = self.asks.keys().next().copied() else {
                        break;
                    };
                    if best_price > limit_price {
                        break;
                    }
                    let queue = self
                        .asks
                        .get_mut(&best_price)
                        .expect("price level exists by construction");
                    fills.push(match_at_level(&order, best_price, queue, &mut remaining));
                    if queue.is_empty() {
                        self.asks.remove(&best_price);
                    }
                }
            }
            Side::Sell => {
                // Sell walks bids from highest; matches while bid >= limit.
                loop {
                    if remaining.0 == 0 {
                        break;
                    }
                    let Some(best_rev) = self.bids.keys().next().copied() else {
                        break;
                    };
                    let best_price = best_rev.0;
                    if best_price < limit_price {
                        break;
                    }
                    let queue = self
                        .bids
                        .get_mut(&best_rev)
                        .expect("price level exists by construction");
                    fills.push(match_at_level(&order, best_price, queue, &mut remaining));
                    if queue.is_empty() {
                        self.bids.remove(&best_rev);
                    }
                }
            }
        }

        // Any unfilled limit qty rests on the book.
        if remaining.0 > 0 {
            let resting = RestingOrder {
                id: order.id,
                account: order.account,
                qty: remaining,
            };
            match order.side {
                Side::Buy => self
                    .bids
                    .entry(Reverse(limit_price))
                    .or_default()
                    .push_back(resting),
                Side::Sell => self.asks.entry(limit_price).or_default().push_back(resting),
            }
            // Limit orders that rest report zero remaining to the caller —
            // the remainder isn't in the return value, it's in the book.
            FillResult {
                fills,
                remaining_qty: Qty(0),
            }
        } else {
            FillResult {
                fills,
                remaining_qty: Qty(0),
            }
        }
    }

    fn submit_market(&mut self, order: Order) -> FillResult {
        let mut remaining = order.qty;
        let mut fills = Vec::new();

        match order.side {
            Side::Buy => loop {
                if remaining.0 == 0 {
                    break;
                }
                let Some(best_price) = self.asks.keys().next().copied() else {
                    break;
                };
                let queue = self
                    .asks
                    .get_mut(&best_price)
                    .expect("price level exists by construction");
                fills.push(match_at_level(&order, best_price, queue, &mut remaining));
                if queue.is_empty() {
                    self.asks.remove(&best_price);
                }
            },
            Side::Sell => loop {
                if remaining.0 == 0 {
                    break;
                }
                let Some(best_rev) = self.bids.keys().next().copied() else {
                    break;
                };
                let queue = self
                    .bids
                    .get_mut(&best_rev)
                    .expect("price level exists by construction");
                fills.push(match_at_level(&order, best_rev.0, queue, &mut remaining));
                if queue.is_empty() {
                    self.bids.remove(&best_rev);
                }
            },
        }

        FillResult {
            fills,
            remaining_qty: remaining,
        }
    }

    /// Cancel a resting order by id. O(n) linear scan; fine for v0 book sizes.
    /// Returns true if the order was found and removed. Empty price levels
    /// left behind by cancellation are also dropped, so `best_bid`/`best_ask`
    /// stay consistent with `depth_bid`/`depth_ask`.
    pub fn cancel(&mut self, order_id: OrderId) -> bool {
        let mut found = false;
        self.bids.retain(|_, queue| {
            if !found && let Some(pos) = queue.iter().position(|o| o.id == order_id) {
                queue.remove(pos);
                found = true;
            }
            !queue.is_empty()
        });
        if found {
            return true;
        }
        self.asks.retain(|_, queue| {
            if !found && let Some(pos) = queue.iter().position(|o| o.id == order_id) {
                queue.remove(pos);
                found = true;
            }
            !queue.is_empty()
        });
        found
    }

    #[must_use]
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next().map(|rp| rp.0)
    }

    #[must_use]
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    /// Best bid price + total qty resting at that price level (sum of every
    /// resting order in the level's FIFO queue). Returns `None` if there
    /// are no bids.
    #[must_use]
    pub fn best_bid_with_qty(&self) -> Option<(Price, Qty)> {
        self.bids.iter().next().map(|(rev_price, queue)| {
            let qty: u64 = queue.iter().map(|o| o.qty.0).sum();
            (rev_price.0, Qty(qty))
        })
    }

    /// Best ask price + total qty resting at that price level.
    #[must_use]
    pub fn best_ask_with_qty(&self) -> Option<(Price, Qty)> {
        self.asks.iter().next().map(|(price, queue)| {
            let qty: u64 = queue.iter().map(|o| o.qty.0).sum();
            (*price, Qty(qty))
        })
    }

    #[must_use]
    pub fn depth_bid(&self) -> usize {
        self.bids.values().map(VecDeque::len).sum()
    }

    #[must_use]
    pub fn depth_ask(&self) -> usize {
        self.asks.values().map(VecDeque::len).sum()
    }
}

/// Match a taker against the front of a single price level.
/// Mutates `queue` (pops the maker if fully filled) and `remaining`.
fn match_at_level(
    taker: &Order,
    price: Price,
    queue: &mut VecDeque<RestingOrder>,
    remaining: &mut Qty,
) -> Fill {
    let maker = queue
        .front_mut()
        .expect("match_at_level called with empty queue");
    let fill_qty = Qty(maker.qty.0.min(remaining.0));

    let fill = Fill {
        maker_order_id: maker.id,
        taker_order_id: taker.id,
        maker_account: maker.account,
        taker_account: taker.account,
        price,
        qty: fill_qty,
        // RestingOrder is on the opposite side of the book from the
        // taker crossing it.
        maker_side: taker.side.opposite(),
    };

    maker.qty.0 -= fill_qty.0;
    remaining.0 -= fill_qty.0;

    if maker.qty.0 == 0 {
        queue.pop_front();
    }

    fill
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(id: u64, account: u64, side: Side, price: u64, qty: u64) -> Order {
        Order {
            id: OrderId(id),
            account: AccountId(account),
            side,
            qty: Qty(qty),
            order_type: OrderType::Limit {
                price: Price(price),
            },
        }
    }

    fn market(id: u64, account: u64, side: Side, qty: u64) -> Order {
        Order {
            id: OrderId(id),
            account: AccountId(account),
            side,
            qty: Qty(qty),
            order_type: OrderType::Market,
        }
    }

    #[test]
    fn empty_book_has_no_best_prices() {
        let book = Book::new();
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.depth_bid(), 0);
        assert_eq!(book.depth_ask(), 0);
    }

    #[test]
    fn resting_limit_creates_bid_or_ask() {
        let mut book = Book::new();
        let r = book.submit(limit(1, 100, Side::Buy, 90, 10));
        assert!(r.fills.is_empty());
        assert_eq!(book.best_bid(), Some(Price(90)));
        assert_eq!(book.best_ask(), None);

        let r = book.submit(limit(2, 101, Side::Sell, 100, 5));
        assert!(r.fills.is_empty());
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn buy_market_takes_best_ask() {
        let mut book = Book::new();
        book.submit(limit(1, 100, Side::Sell, 100, 5));
        book.submit(limit(2, 101, Side::Sell, 105, 5));

        let r = book.submit(market(99, 200, Side::Buy, 8));
        assert_eq!(r.fills.len(), 2);
        assert_eq!(r.fills[0].price, Price(100)); // best ask first
        assert_eq!(r.fills[0].qty, Qty(5));
        assert_eq!(r.fills[1].price, Price(105));
        assert_eq!(r.fills[1].qty, Qty(3));
        assert_eq!(r.remaining_qty, Qty(0));
        assert_eq!(book.depth_ask(), 1); // ask @ 105 has 2 left
    }

    #[test]
    fn limit_buy_walks_asks_within_price() {
        let mut book = Book::new();
        book.submit(limit(1, 100, Side::Sell, 100, 5));
        book.submit(limit(2, 101, Side::Sell, 105, 5));

        // Buy limit @ 103 — should only fill the 100-priced level.
        let r = book.submit(limit(99, 200, Side::Buy, 103, 10));
        assert_eq!(r.fills.len(), 1);
        assert_eq!(r.fills[0].price, Price(100));
        assert_eq!(r.fills[0].qty, Qty(5));
        // Remainder rests as a bid @ 103.
        assert_eq!(book.best_bid(), Some(Price(103)));
        assert_eq!(book.depth_bid(), 1);
    }

    #[test]
    fn price_time_priority_within_level() {
        let mut book = Book::new();
        book.submit(limit(1, 100, Side::Sell, 100, 5)); // first
        book.submit(limit(2, 101, Side::Sell, 100, 5)); // same price, later

        let r = book.submit(market(99, 200, Side::Buy, 7));
        assert_eq!(r.fills.len(), 2);
        assert_eq!(r.fills[0].maker_order_id, OrderId(1)); // first in, first out
        assert_eq!(r.fills[0].qty, Qty(5));
        assert_eq!(r.fills[1].maker_order_id, OrderId(2));
        assert_eq!(r.fills[1].qty, Qty(2));
    }

    #[test]
    fn market_with_insufficient_liquidity_returns_remaining() {
        let mut book = Book::new();
        book.submit(limit(1, 100, Side::Sell, 100, 3));

        let r = book.submit(market(99, 200, Side::Buy, 10));
        assert_eq!(r.fills.len(), 1);
        assert_eq!(r.fills[0].qty, Qty(3));
        assert_eq!(r.remaining_qty, Qty(7)); // market discards remainder
        assert_eq!(book.depth_ask(), 0);
    }

    #[test]
    fn cancel_removes_resting_order() {
        let mut book = Book::new();
        book.submit(limit(1, 100, Side::Buy, 90, 10));
        assert_eq!(book.depth_bid(), 1);

        assert!(book.cancel(OrderId(1)));
        assert_eq!(book.depth_bid(), 0);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn cancel_unknown_returns_false() {
        let mut book = Book::new();
        assert!(!book.cancel(OrderId(999)));
    }

    #[test]
    fn book_does_not_cross_after_match() {
        let mut book = Book::new();
        book.submit(limit(1, 100, Side::Sell, 100, 5));
        book.submit(limit(2, 101, Side::Buy, 95, 5));
        // Spread: bid 95, ask 100. No cross.
        let bid = book.best_bid().unwrap();
        let ask = book.best_ask().unwrap();
        assert!(bid < ask);

        // Now a buy @ 100 — fully fills, no resting.
        book.submit(limit(3, 102, Side::Buy, 100, 5));
        // Best bid is still 95 (from order 2). Ask is gone.
        assert_eq!(book.best_bid(), Some(Price(95)));
        assert_eq!(book.best_ask(), None);
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    /// A simplified action enum for property-based testing.
    #[derive(Clone, Debug)]
    enum Action {
        SubmitLimit {
            id: u64,
            account: u64,
            side: Side,
            price: u64,
            qty: u64,
        },
        SubmitMarket {
            id: u64,
            account: u64,
            side: Side,
            qty: u64,
        },
    }

    fn arb_side() -> impl Strategy<Value = Side> {
        prop_oneof![Just(Side::Buy), Just(Side::Sell)]
    }

    fn arb_action(id: u64) -> impl Strategy<Value = Action> {
        let limit_action = (1u64..=200, 1u64..=20, arb_side(), 50u64..=150)
            .prop_map(move |(account, qty, side, price)| Action::SubmitLimit {
                id,
                account,
                side,
                price,
                qty,
            });
        let market_action = (1u64..=200, 1u64..=20, arb_side()).prop_map(
            move |(account, qty, side)| Action::SubmitMarket {
                id,
                account,
                side,
                qty,
            },
        );
        prop_oneof![3 => limit_action, 1 => market_action]
    }

    fn arb_actions() -> impl Strategy<Value = Vec<Action>> {
        prop::collection::vec(0u64..1000, 1..30)
            .prop_flat_map(|ids| {
                ids.into_iter()
                    .enumerate()
                    .map(|(i, _)| arb_action(i as u64 + 1))
                    .collect::<Vec<_>>()
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        })]

        /// Quantity is conserved: every fill_qty came from a resting maker;
        /// total qty in/out balances.
        #[test]
        fn qty_conservation(actions in arb_actions()) {
            let mut book = Book::new();
            let mut total_in = 0u64;
            let mut total_filled = 0u64;
            let mut total_market_unfilled = 0u64;

            for action in actions {
                match action {
                    Action::SubmitLimit { id, account, side, price, qty } => {
                        total_in += qty;
                        let r = book.submit(Order {
                            id: OrderId(id),
                            account: AccountId(account),
                            side,
                            qty: Qty(qty),
                            order_type: OrderType::Limit { price: Price(price) },
                        });
                        total_filled += r.total_filled().0;
                    }
                    Action::SubmitMarket { id, account, side, qty } => {
                        total_in += qty;
                        let r = book.submit(Order {
                            id: OrderId(id),
                            account: AccountId(account),
                            side,
                            qty: Qty(qty),
                            order_type: OrderType::Market,
                        });
                        total_filled += r.total_filled().0;
                        total_market_unfilled += r.remaining_qty.0;
                    }
                }
            }

            // Resting quantity = total_in - 2*total_filled - total_market_unfilled.
            // (Each fill consumes one unit from a maker AND one unit from a taker,
            // so total_filled counts qty, but the qty appeared in total_in twice
            // — once when the maker was submitted, once when the taker arrived.)
            let resting: u64 = book.bids.values()
                .flat_map(|q| q.iter())
                .chain(book.asks.values().flat_map(|q| q.iter()))
                .map(|o| o.qty.0)
                .sum();
            prop_assert_eq!(total_in, 2 * total_filled + total_market_unfilled + resting);
        }

        /// Book invariant: best bid is strictly less than best ask. The book
        /// should never be crossed after submit() completes.
        #[test]
        fn no_crossed_book(actions in arb_actions()) {
            let mut book = Book::new();
            for action in actions {
                match action {
                    Action::SubmitLimit { id, account, side, price, qty } => {
                        book.submit(Order {
                            id: OrderId(id),
                            account: AccountId(account),
                            side,
                            qty: Qty(qty),
                            order_type: OrderType::Limit { price: Price(price) },
                        });
                    }
                    Action::SubmitMarket { id, account, side, qty } => {
                        book.submit(Order {
                            id: OrderId(id),
                            account: AccountId(account),
                            side,
                            qty: Qty(qty),
                            order_type: OrderType::Market,
                        });
                    }
                }
                if let (Some(b), Some(a)) = (book.best_bid(), book.best_ask()) {
                    prop_assert!(b < a, "book crossed: bid={} ask={}", b.0, a.0);
                }
            }
        }

        /// Determinism: applying the same action sequence produces the same
        /// book + fill history every time. (The "replayability" property
        /// from the architecture doc — required for consensus determinism.)
        #[test]
        fn determinism(actions in arb_actions()) {
            let run = |actions: &[Action]| {
                let mut book = Book::new();
                let mut all_fills: Vec<Fill> = Vec::new();
                for action in actions {
                    let order = match action {
                        Action::SubmitLimit { id, account, side, price, qty } => Order {
                            id: OrderId(*id),
                            account: AccountId(*account),
                            side: *side,
                            qty: Qty(*qty),
                            order_type: OrderType::Limit { price: Price(*price) },
                        },
                        Action::SubmitMarket { id, account, side, qty } => Order {
                            id: OrderId(*id),
                            account: AccountId(*account),
                            side: *side,
                            qty: Qty(*qty),
                            order_type: OrderType::Market,
                        },
                    };
                    all_fills.extend(book.submit(order).fills);
                }
                (book.best_bid(), book.best_ask(), book.depth_bid(), book.depth_ask(), all_fills)
            };
            prop_assert_eq!(run(&actions), run(&actions));
        }
    }
}
