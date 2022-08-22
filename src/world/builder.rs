use core::sync::atomic::AtomicU64;

use crate::{
    action::ActionEncoder,
    component::{
        Component, ComponentInfo, ComponentInfoRef, ComponentRegistry, ExternalDropHook,
        ExternalSetHook,
    },
    entity::Entities,
    res::Res,
};

use super::{ArchetypeSet, Edges, World};

/// Builder for [`World`] value.
///
/// [`WorldBuilder`] allows to perform setup before building [`World`] value.
/// That otherwise would be impossible.
/// For example [`WorldBuilder::register_component`] allows customization of registered components.
pub struct WorldBuilder {
    registry: ComponentRegistry,
}

impl WorldBuilder {
    /// Returns new [`WorldBuilder`] value.
    pub const fn new() -> WorldBuilder {
        WorldBuilder {
            registry: ComponentRegistry::new(),
        }
    }

    /// Returns newly created [`World`] with configuration copied from this [`WorldBuilder`].
    pub fn build(self) -> World {
        World {
            epoch: AtomicU64::new(0),
            entities: Entities::new(),
            archetypes: ArchetypeSet::new(),
            edges: Edges::new(),
            res: Res::new(),
            registry: self.registry,
            cached_encoder: Some(ActionEncoder::new()),
        }
    }

    /// Registers new component type and allows modifying it.
    pub fn register_raw(&mut self, info: ComponentInfo) {
        self.registry.register_raw(info);
    }

    /// Registers new component type and allows modifying it.
    pub fn register_component<T>(&mut self) -> ComponentInfoRef<'_, T>
    where
        T: Component,
    {
        self.registry.register_component::<T>()
    }

    /// Registers new component type and allows modifying it.
    pub fn register_external<T>(
        &mut self,
    ) -> ComponentInfoRef<'_, T, ExternalDropHook, ExternalSetHook>
    where
        T: 'static,
    {
        self.registry.register_external::<T>()
    }
}
