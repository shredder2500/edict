use core::ops::Range;

use crate::{
    archetype::{chunk_idx, first_of_chunk, Archetype, CHUNK_LEN},
    epoch::EpochId,
    query::{AsQuery, Fetch, ImmutableQuery, Query, QueryItem},
};

use super::{BorrowState, RuntimeBorrowState, StaticallyBorrowed, ViewValue};

/// Iterator over entities with a query `Q` and filter `F`.
/// Yields query items for every entity matching both the query and the filter.
///
/// Component borrow is acquired before construction.
pub type ViewIter<'a, Q, F> =
    ViewValueIter<'a, <Q as AsQuery>::Query, <F as AsQuery>::Query, StaticallyBorrowed>;

/// Iterator over entities with a query `Q` and filter `F`.
/// Yields query items for every entity matching both the query and the filter.
///
/// Component borrow is acquired on construction and released when iterator is dropped.
pub type ViewCellIter<'a, Q, F> =
    ViewValueIter<'a, <Q as AsQuery>::Query, <F as AsQuery>::Query, RuntimeBorrowState>;

impl<'a, Q, F, B, E> ViewValue<'a, Q, F, B, E>
where
    Q: Query,
    F: Query,
    B: BorrowState,
{
    /// Returns an iterator over entities with a query `Q` and filter `F`.
    ///
    /// Unlike `iter`, this version works for views with mutable queries
    /// since mutable borrow won't allow to iterate the view multiple times simultaneously.
    #[inline(always)]
    pub fn iter_mut(&mut self) -> ViewValueIter<'_, Q, F, StaticallyBorrowed> {
        let epoch = self.epochs.next_if(Q::MUTABLE || F::MUTABLE);

        self.acquire_borrow();

        // Safety: we just acquired the borrow. Releasing requires a mutable reference to self.
        // This ensures that it can only happen after the iterator is dropped.
        unsafe {
            ViewValueIter::new(
                epoch,
                self.query,
                self.filter,
                self.archetypes,
                StaticallyBorrowed,
            )
        }
    }
}

impl<'a, Q, F, B, E> ViewValue<'a, Q, F, B, E>
where
    Q: ImmutableQuery,
    F: ImmutableQuery,
    B: BorrowState,
{
    /// Returns an iterator over entities with a query `Q` and filter `F`.
    ///
    /// Unlike `iter_mut`, this version only works for views with immutable queries.
    /// Immutable query are guaranteed to not conflict with any other immutable query,
    /// allowing for iterating a view multiple times simultaneously.
    #[inline(always)]
    pub fn iter(&self) -> ViewValueIter<'_, Q, F, StaticallyBorrowed> {
        debug_assert!(!Q::MUTABLE && !F::MUTABLE);
        let epoch = self.epochs.current();

        self.acquire_borrow();

        // Safety: we just acquired the borrow. Releasing requires a mutable reference to self.
        // This ensures that it can only happen after the iterator is dropped.
        unsafe {
            ViewValueIter::new(
                epoch,
                self.query,
                self.filter,
                self.archetypes,
                StaticallyBorrowed,
            )
        }
    }
}

impl<'a, Q, F, B, E> IntoIterator for ViewValue<'a, Q, F, B, E>
where
    Q: Query,
    F: Query,
    B: BorrowState,
{
    type Item = QueryItem<'a, Q>;
    type IntoIter = ViewValueIter<'a, Q, F, B>;

    #[inline(always)]
    fn into_iter(self) -> ViewValueIter<'a, Q, F, B> {
        let epoch = self.epochs.next_if(Q::MUTABLE || F::MUTABLE);

        let query = self.query;
        let filter = self.filter;
        let archetypes = self.archetypes;
        let (state, _) = self.extract();

        // Safety: Existence of this ViewValue guarantees that the borrow state is valid.
        // Borrow state is given to the iter where it will be released on drop.
        unsafe { ViewValueIter::new(epoch, query, filter, archetypes, state) }
    }
}

/// Iterator over entities with a query `Q`.
/// Yields query items for every matching entity.
pub struct ViewValueIter<'a, Q: Query, F: Query, B: BorrowState> {
    query: Q,
    filter: F,
    query_fetch: Q::Fetch<'a>,
    filter_fetch: F::Fetch<'a>,
    epoch: EpochId,
    archetypes: &'a [Archetype],
    next_archetype: usize,
    indices: Range<u32>,
    touch_chunk: bool,
    state: B,
}

impl<Q, F, B> Drop for ViewValueIter<'_, Q, F, B>
where
    Q: Query,
    F: Query,
    B: BorrowState,
{
    fn drop(&mut self) {
        self.state.release(self.query, self.filter, self.archetypes);
    }
}

impl<'a, Q, F, B> ViewValueIter<'a, Q, F, B>
where
    Q: Query,
    F: Query,
    B: BorrowState,
{
    /// Creates a new iterator over entities with a query `Q` and filter `F`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function
    /// is never called for the same archetypes with static borrow state
    /// and any other borrow state for conflicting queries.
    unsafe fn new(
        epoch: EpochId,
        query: Q,
        filter: F,
        archetypes: &'a [Archetype],
        state: B,
    ) -> Self {
        state.acquire(query, filter, archetypes);

        ViewValueIter {
            query,
            filter,
            query_fetch: Fetch::dangling(),
            filter_fetch: Fetch::dangling(),
            epoch,
            next_archetype: 0,
            indices: 0..0,
            touch_chunk: false,
            state,
            archetypes,
        }
    }
}

impl<'a, Q, F, B> Iterator for ViewValueIter<'a, Q, F, B>
where
    Q: Query,
    F: Query,
    B: BorrowState,
{
    type Item = QueryItem<'a, Q>;

    #[inline(always)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let upper = self.archetypes[self.next_archetype..].iter().fold(
            self.indices.len(),
            |acc, archetype| {
                if !self.filter.visit_archetype(archetype)
                    || !unsafe { self.filter.visit_archetype_late(archetype) }
                {
                    return acc;
                }
                if !self.query.visit_archetype(archetype)
                    || !unsafe { self.query.visit_archetype_late(archetype) }
                {
                    return acc;
                }
                acc + archetype.len()
            },
        );

        (0, Some(upper))
    }

    #[inline(always)]
    fn next(&mut self) -> Option<QueryItem<'a, Q>> {
        loop {
            match self.indices.next() {
                None => {
                    // move to the next archetype.
                    loop {
                        if self.next_archetype >= self.archetypes.len() {
                            return None;
                        }
                        let arch_idx = self.next_archetype;
                        self.next_archetype += 1;
                        let archetype = &self.archetypes[arch_idx];

                        if archetype.is_empty() {
                            continue;
                        }

                        if !self.filter.visit_archetype(archetype)
                            || !unsafe { self.filter.visit_archetype_late(archetype) }
                        {
                            continue;
                        }

                        if !self.query.visit_archetype(archetype)
                            || !unsafe { self.query.visit_archetype_late(archetype) }
                        {
                            continue;
                        }

                        self.filter_fetch =
                            unsafe { self.filter.fetch(arch_idx as u32, archetype, self.epoch) };
                        self.query_fetch =
                            unsafe { self.query.fetch(arch_idx as u32, archetype, self.epoch) };
                        self.indices = 0..archetype.len() as u32;
                        break;
                    }
                }
                Some(entity_idx) => {
                    if let Some(chunk_idx) = first_of_chunk(entity_idx) {
                        if !unsafe { self.filter_fetch.visit_chunk(chunk_idx) } {
                            self.indices.nth(CHUNK_LEN as usize - 1);
                            continue;
                        }
                        if !unsafe { self.query_fetch.visit_chunk(chunk_idx) } {
                            self.indices.nth(CHUNK_LEN as usize - 1);
                            continue;
                        }
                        self.touch_chunk = true;
                    }

                    if !unsafe { self.filter_fetch.visit_item(entity_idx) } {
                        continue;
                    }

                    if !unsafe { self.query_fetch.visit_item(entity_idx) } {
                        continue;
                    }

                    if self.touch_chunk {
                        unsafe { self.filter_fetch.touch_chunk(chunk_idx(entity_idx)) }
                        unsafe { self.query_fetch.touch_chunk(chunk_idx(entity_idx)) }
                        self.touch_chunk = false;
                    }

                    let item = unsafe { self.query_fetch.get_item(entity_idx) };

                    return Some(item);
                }
            }
        }
    }

    fn fold<I, Fun>(mut self, init: I, mut f: Fun) -> I
    where
        Self: Sized,
        Fun: FnMut(I, QueryItem<'a, Q>) -> I,
    {
        let mut acc = init;
        while let Some(entity_idx) = self.indices.next() {
            if let Some(chunk_idx) = first_of_chunk(entity_idx) {
                if !unsafe { self.filter_fetch.visit_chunk(chunk_idx) } {
                    self.indices.nth(CHUNK_LEN as usize - 1);
                    continue;
                }
                if !unsafe { self.query_fetch.visit_chunk(chunk_idx) } {
                    self.indices.nth(CHUNK_LEN as usize - 1);
                    continue;
                }
                self.touch_chunk = true;
            }

            if !unsafe { self.filter_fetch.visit_item(entity_idx) } {
                continue;
            }
            if !unsafe { self.query_fetch.visit_item(entity_idx) } {
                continue;
            }

            if self.touch_chunk {
                unsafe { self.filter_fetch.touch_chunk(chunk_idx(entity_idx)) }
                unsafe { self.query_fetch.touch_chunk(chunk_idx(entity_idx)) }
                self.touch_chunk = false;
            }
            let item = unsafe { self.query_fetch.get_item(entity_idx) };

            acc = f(acc, item);
        }

        for arch_idx in self.next_archetype..self.archetypes.len() {
            let archetype = &self.archetypes[arch_idx];
            if archetype.is_empty() {
                continue;
            }
            if !self.filter.visit_archetype(archetype)
                || !unsafe { self.filter.visit_archetype_late(archetype) }
            {
                continue;
            }
            if !self.query.visit_archetype(archetype)
                || !unsafe { self.query.visit_archetype_late(archetype) }
            {
                continue;
            }
            let mut filter_fetch =
                unsafe { self.filter.fetch(arch_idx as u32, archetype, self.epoch) };
            let mut query_fetch =
                unsafe { self.query.fetch(arch_idx as u32, archetype, self.epoch) };

            let mut indices = 0..archetype.len() as u32;

            while let Some(entity_idx) = indices.next() {
                if let Some(chunk_idx) = first_of_chunk(entity_idx) {
                    if !unsafe { query_fetch.visit_chunk(chunk_idx) } {
                        self.indices.nth(CHUNK_LEN as usize - 1);
                        continue;
                    }
                    if !unsafe { filter_fetch.visit_chunk(chunk_idx) } {
                        self.indices.nth(CHUNK_LEN as usize - 1);
                        continue;
                    }
                    self.touch_chunk = true;
                }

                if !unsafe { filter_fetch.visit_item(entity_idx) } {
                    continue;
                }
                if !unsafe { query_fetch.visit_item(entity_idx) } {
                    continue;
                }

                if self.touch_chunk {
                    unsafe { filter_fetch.touch_chunk(chunk_idx(entity_idx)) }
                    unsafe { query_fetch.touch_chunk(chunk_idx(entity_idx)) }
                    self.touch_chunk = false;
                }
                let item = unsafe { query_fetch.get_item(entity_idx) };

                acc = f(acc, item);
            }
        }
        acc
    }
}
