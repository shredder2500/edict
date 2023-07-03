use alloc::collections::VecDeque;

use crate::world::World;

use super::{ActionEncoder, ActionFn};

/// Buffer with all actions recorded by [`ActionEncoder`].
#[derive(Default)]
#[repr(transparent)]
pub struct ActionBuffer {
    actions: VecDeque<ActionFn<'static>>,
}

impl ActionBuffer {
    /// Returns new empty action buffer.
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            actions: VecDeque::new(),
        }
    }

    pub(super) fn actions(&mut self) -> &mut VecDeque<ActionFn<'static>> {
        &mut self.actions
    }

    /// Returns an encoder that records actions into this buffer.
    ///
    /// Actions should be executed on the same [`World`],
    /// otherwise entity ids will not refer to the correct entities.
    #[inline(always)]
    pub fn encoder<'a>(&'a mut self, world: &'a World) -> ActionEncoder<'a> {
        ActionEncoder::new(self, world.entities())
    }

    /// Executes recorded actions onto the [`World`].
    /// Iterates through all recorded actions and executes them one by one.
    /// Executed actions may trigger component hooks.
    /// Hooks record actions into the same buffer.
    ///
    /// After execution buffer is empty.
    /// Actions recorded during execution are executed as well.
    ///
    /// An infinite recursion is possible if a hook records an action that
    /// transitively triggers the same hook again.
    ///
    /// Returns `true` if at least one action was executed.
    #[inline(always)]
    pub fn execute(&mut self, world: &mut World) -> bool {
        if self.actions.is_empty() {
            return false;
        }

        while let Some(fun) = self.actions.pop_front() {
            fun.call(world, self);
        }

        true
    }
}

/// Extension trait for slice of [`ActionBuffer`]s.
pub trait ActionBufferSliceExt {
    /// Execute all action encoders from the slice.
    /// Returns `true` if at least one action was executed.
    fn execute_all(&mut self, world: &mut World) -> bool;
}

impl ActionBufferSliceExt for [ActionBuffer] {
    fn execute_all(&mut self, world: &mut World) -> bool {
        self.iter_mut().any(|encoder| encoder.execute(world))
    }
}
