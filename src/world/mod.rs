//! Self-contained ECS [`World`].

use core::{
    any::{type_name, TypeId},
    fmt,
    hash::Hash,
    iter::FromIterator,
    iter::FusedIterator,
    marker::PhantomData,
};

use alloc::vec::Vec;

use crate::{
    action::ActionEncoder,
    archetype::{chunk_idx, Archetype},
    bundle::{
        Bundle, BundleDesc, ComponentBundle, ComponentBundleDesc, DynamicBundle,
        DynamicComponentBundle,
    },
    component::{Component, ComponentInfo, ComponentRegistry},
    entity::{Entities, EntityId},
    query::{Fetch, PhantomQuery, PhantomQueryItem, Query, QueryItem},
    relation::{OriginComponent, Relation, TargetComponent},
};

use self::edges::Edges;

pub use self::{
    builder::WorldBuilder,
    meta::EntityMeta,
    query::{QueryMut, QueryRef},
};

mod builder;
mod edges;
mod meta;
mod query;

/// Limits on reserving of space for entities and components
/// in archetypes when `spawn_batch` is used.
const MAX_SPAWN_RESERVE: usize = 1024;

fn spawn_reserve(iter: &impl Iterator, archetype: &mut Archetype) {
    let (lower, upper) = iter.size_hint();
    let additional = match (lower, upper) {
        (lower, None) => lower,
        (lower, Some(upper)) => {
            // Iterator is consumed in full, so reserve at least `lower`.
            lower.max(upper.min(MAX_SPAWN_RESERVE))
        }
    };
    archetype.reserve(additional);
}

/// Container for entities with any sets of components.
///
/// Entities can be spawned in the `World` with handle `Entity` returned,
/// that can be used later to access that entity.
///
/// `Entity` handle can be downgraded to `EntityId`.
///
/// Entity would be despawned after last `Entity` is dropped.
///
/// Entity's set of components may be modified in any way.
///
/// Entities can be fetched directly, using `Entity` or `EntityId`
/// with different guarantees and requirements.
///
/// Entities can be efficiently queried from `World` to iterate over all entities
/// that match query requirements.
///
/// Internally `World` manages entities generations,
/// maps entity to location of components in archetypes,
/// moves components of entities between archetypes,
/// spawns and despawns entities.
#[allow(missing_debug_implementations)]
pub struct World {
    /// Global epoch counter of the World.
    /// Incremented on each mutable query.
    epoch: u64,

    /// Collection of entities with their locations.
    entities: Entities,

    /// Archetypes of entities in the world.
    archetypes: Vec<Archetype>,

    edges: Edges,

    registry: ComponentRegistry,

    /// Internal action encoder.
    /// This encoder is used to record commands from component hooks.
    /// Commands are immediately executed at the end of the mutating call.
    cached_encoder: Option<ActionEncoder>,
}

impl Default for World {
    fn default() -> Self {
        World::new()
    }
}

macro_rules! with_encoder {
    ($world:ident, $encoder:ident => $expr:expr) => {{
        let mut $encoder = $world
            .cached_encoder
            .take()
            .unwrap_or_else(ActionEncoder::new);
        let result = $expr;
        ActionEncoder::execute(&mut $encoder, $world);
        $world.cached_encoder = Some($encoder);
        result
    }};
}

impl World {
    /// Returns new instance of [`WorldBuilder`]
    pub const fn builder() -> WorldBuilder {
        WorldBuilder::new()
    }

    /// Returns new instance of [`World`].
    ///
    /// Created [`World`] instance contains no entities.
    ///
    /// Internal caches that make operations faster are empty.
    /// This can make a small spike in latency
    /// as each cache entry would be calculated on first use of each key.
    #[inline]
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Spawns new entity in this world with provided bundle of components.
    /// World keeps ownership of the spawned entity and entity id is returned.
    #[inline]
    pub fn spawn<B>(&mut self, bundle: B) -> EntityId
    where
        B: DynamicComponentBundle,
    {
        self.spawn_impl(bundle, register_bundle::<B>)
    }

    /// Spawns new entity in this world with provided bundle of components.
    /// World keeps ownership of the spawned entity and entity id is returned.
    #[inline]
    pub fn spawn_external<B>(&mut self, bundle: B) -> EntityId
    where
        B: DynamicBundle,
    {
        self.spawn_impl(bundle, assert_registered_bundle::<B>)
    }

    fn spawn_impl<B, F>(&mut self, bundle: B, register_bundle: F) -> EntityId
    where
        B: DynamicBundle,
        F: FnOnce(&mut ComponentRegistry, &B),
    {
        if !bundle.valid() {
            panic!(
                "Specified bundle `{}` is not valid. Check for duplicate component types",
                type_name::<B>()
            );
        }

        let entity = self.entities.spawn();

        let archetype_idx = self.edges.spawn(
            &mut self.registry,
            &mut self.archetypes,
            &bundle,
            |registry| register_bundle(registry, &bundle),
        );

        self.epoch += 1;
        let idx = self.archetypes[archetype_idx as usize].spawn(entity, bundle, self.epoch);
        self.entities.set_location(entity.idx(), archetype_idx, idx);
        entity
    }

    /// Returns an iterator which spawns and yield entities
    /// using bundles returnd from provided iterator.
    ///
    /// When bundles iterator returns `None`, returned iterator returns `None` too.
    ///
    /// If bundles iterator is fused, returned iterator is fused too.
    /// If bundles iterator is double-ended, returned iterator is double-ended too.
    /// If bundles iterator has exact size, returned iterator has exact size too.
    ///
    /// Skipping items on returned iterator will not cause them to be spawned
    /// and same number of items will be skipped on bundles iterator.
    ///
    /// Returned iterator attempts to optimize storage allocation for entities
    /// if consumed with functions like `fold`, `rfold`, `for_each` or `collect`.
    ///
    /// When returned iterator is dropped, no more entities will be spawned
    /// even if bundles iterator has items left.
    #[inline]
    pub fn spawn_batch<B, I>(&mut self, bundles: I) -> SpawnBatch<'_, I::IntoIter>
    where
        I: IntoIterator<Item = B>,
        B: ComponentBundle,
    {
        self.spawn_batch_impl(bundles, |registry| {
            register_bundle(registry, &PhantomData::<B>)
        })
    }

    /// Returns an iterator which spawns and yield entities
    /// using bundles returnd from provided iterator.
    ///
    /// When bundles iterator returns `None`, returned iterator returns `None` too.
    ///
    /// If bundles iterator is fused, returned iterator is fused too.
    /// If bundles iterator is double-ended, returned iterator is double-ended too.
    /// If bundles iterator has exact size, returned iterator has exact size too.
    ///
    /// Skipping items on returned iterator will not cause them to be spawned
    /// and same number of items will be skipped on bundles iterator.
    ///
    /// Returned iterator attempts to optimize storage allocation for entities
    /// if consumed with functions like `fold`, `rfold`, `for_each` or `collect`.
    ///
    /// When returned iterator is dropped, no more entities will be spawned
    /// even if bundles iterator has items left.
    #[inline]
    pub fn spawn_batch_external<B, I>(&mut self, bundles: I) -> SpawnBatch<'_, I::IntoIter>
    where
        I: IntoIterator<Item = B>,
        B: Bundle,
    {
        self.spawn_batch_impl(bundles, |registry| {
            assert_registered_bundle(registry, &PhantomData::<B>)
        })
    }

    fn spawn_batch_impl<B, I, F>(
        &mut self,
        bundles: I,
        register_bundle: F,
    ) -> SpawnBatch<'_, I::IntoIter>
    where
        I: IntoIterator<Item = B>,
        B: Bundle,
        F: FnOnce(&mut ComponentRegistry),
    {
        if !B::static_valid() {
            panic!(
                "Specified bundle `{}` is not valid. Check for duplicate component types",
                type_name::<B>()
            );
        }

        let archetype_idx = self.edges.insert_bundle(
            &mut self.registry,
            &mut self.archetypes,
            0,
            &PhantomData::<I::Item>,
            register_bundle,
        );

        self.epoch += 1;

        let archetype = &mut self.archetypes[archetype_idx as usize];
        let entities = &mut self.entities;
        let epoch = self.epoch;

        SpawnBatch {
            bundles: bundles.into_iter(),
            epoch,
            archetype_idx,
            archetype,
            entities,
        }
    }

    /// Despawns an entity with specified id.
    #[inline]
    pub fn despawn(&mut self, entity: EntityId) -> Result<(), NoSuchEntity> {
        with_encoder!(self, encoder => self.despawn_with_encoder(entity, &mut encoder))
    }

    pub(crate) fn despawn_with_encoder(
        &mut self,
        entity: EntityId,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity> {
        let (archetype, idx) = self.entities.despawn(entity)?;

        let opt_id =
            unsafe { self.archetypes[archetype as usize].despawn_unchecked(entity, idx, encoder) };
        if let Some(id) = opt_id {
            self.entities.set_location(id, archetype, idx)
        }

        Ok(())
    }

    /// Searches for an entity with specified index.
    /// Returns `Ok(entity)` if entity with specified index exists.
    /// Returns `Err(NoSuchEntity)` otherwise.
    #[inline]
    pub fn find_entity(&self, idx: u32) -> Result<EntityId, NoSuchEntity> {
        self.entities.find_entity(idx).ok_or(NoSuchEntity)
    }

    /// Attempts to inserts component to the specified entity.
    ///
    /// If entity already had component of that type,
    /// old component value is replaced with new one.
    /// Otherwise new component is added to the entity.
    ///
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn insert<T>(&mut self, entity: EntityId, component: T) -> Result<(), NoSuchEntity>
    where
        T: Component,
    {
        with_encoder!(self, encoder => self.insert_with_encoder(entity, component, &mut encoder))
    }

    /// Attempts to inserts component to the specified entity.
    ///
    /// If entity already had component of that type,
    /// old component value is replaced with new one.
    /// Otherwise new component is added to the entity.
    ///
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn insert_external<T>(&mut self, entity: EntityId, component: T) -> Result<(), NoSuchEntity>
    where
        T: 'static,
    {
        with_encoder!(self, encoder => self.insert_with_encoder_external(entity, component, &mut encoder))
    }

    #[inline]
    pub(crate) fn insert_with_encoder<T>(
        &mut self,
        entity: EntityId,
        component: T,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        T: Component,
    {
        self.insert_with_encoder_impl(entity, component, encoder, register_one::<T>)
    }

    #[inline]
    pub(crate) fn insert_with_encoder_external<T>(
        &mut self,
        entity: EntityId,
        component: T,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        T: 'static,
    {
        self.insert_with_encoder_impl(entity, component, encoder, assert_registered_one::<T>)
    }

    #[inline]
    pub(crate) fn insert_with_encoder_impl<T, F>(
        &mut self,
        entity: EntityId,
        component: T,
        encoder: &mut ActionEncoder,
        get_or_register: F,
    ) -> Result<(), NoSuchEntity>
    where
        T: 'static,
        F: FnOnce(&mut ComponentRegistry) -> &ComponentInfo,
    {
        let (src_archetype, idx) = self.entities.get(entity).ok_or(NoSuchEntity)?;

        self.epoch += 1;

        if self.archetypes[src_archetype as usize].contains_id(TypeId::of::<T>()) {
            unsafe {
                self.archetypes[src_archetype as usize]
                    .set(entity, idx, component, self.epoch, encoder);
            }

            return Ok(());
        }

        let dst_archetype = self.edges.insert(
            TypeId::of::<T>(),
            &mut self.registry,
            &mut self.archetypes,
            src_archetype,
            get_or_register,
        );

        debug_assert_ne!(src_archetype, dst_archetype);

        let (before, after) = self
            .archetypes
            .split_at_mut(src_archetype.max(dst_archetype) as usize);

        let (src, dst) = match src_archetype < dst_archetype {
            true => (&mut before[src_archetype as usize], &mut after[0]),
            false => (&mut after[0], &mut before[dst_archetype as usize]),
        };

        let (dst_idx, opt_src_id) = unsafe { src.insert(entity, dst, idx, component, self.epoch) };

        self.entities
            .set_location(entity.idx(), dst_archetype, dst_idx);

        if let Some(src_id) = opt_src_id {
            self.entities.set_location(src_id, src_archetype, idx);
        }

        Ok(())
    }

    /// Removes component from the specified entity and returns its value.
    ///
    /// If entity does not have component of this type, fails with `Err(EntityError::MissingComponent)`.
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn remove<T>(&mut self, entity: EntityId) -> Result<T, EntityError>
    where
        T: 'static,
    {
        let (src_archetype, idx) = self.entities.get(entity).ok_or(EntityError::NoSuchEntity)?;

        self.epoch += 1;

        if !self.archetypes[src_archetype as usize].contains_id(TypeId::of::<T>()) {
            return Err(EntityError::MissingComponents);
        }

        let dst_archetype =
            self.edges
                .remove(&mut self.archetypes, src_archetype, TypeId::of::<T>());

        debug_assert_ne!(src_archetype, dst_archetype);

        let (before, after) = self
            .archetypes
            .split_at_mut(src_archetype.max(dst_archetype) as usize);

        let (src, dst) = match src_archetype < dst_archetype {
            true => (&mut before[src_archetype as usize], &mut after[0]),
            false => (&mut after[0], &mut before[dst_archetype as usize]),
        };

        let (dst_idx, opt_src_id, component) = unsafe { src.remove(entity, dst, idx) };

        self.entities
            .set_location(entity.idx(), dst_archetype, dst_idx);

        if let Some(src_id) = opt_src_id {
            self.entities.set_location(src_id, src_archetype, idx);
        }

        Ok(component)
    }

    /// Drops component from the specified entity.
    ///
    /// If entity does not have component of this type, fails with `Err(EntityError::MissingComponent)`.
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn drop<T>(&mut self, entity: EntityId) -> Result<(), EntityError>
    where
        T: 'static,
    {
        self.drop_erased(entity, TypeId::of::<T>())
    }

    /// Drops component from the specified entity.
    ///
    /// If entity does not have component of this type, fails with `Err(EntityError::MissingComponent)`.
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn drop_erased(&mut self, entity: EntityId, id: TypeId) -> Result<(), EntityError> {
        with_encoder!(self, encoder => self.drop_erased_with_encoder(entity, id, &mut encoder))
    }

    pub(crate) fn drop_erased_with_encoder(
        &mut self,
        entity: EntityId,
        id: TypeId,
        encoder: &mut ActionEncoder,
    ) -> Result<(), EntityError> {
        let (src_archetype, idx) = self.entities.get(entity).ok_or(EntityError::NoSuchEntity)?;

        self.epoch += 1;

        if !self.archetypes[src_archetype as usize].contains_id(id) {
            return Err(EntityError::MissingComponents);
        }

        let dst_archetype = self.edges.remove(&mut self.archetypes, src_archetype, id);

        debug_assert_ne!(src_archetype, dst_archetype);

        let (before, after) = self
            .archetypes
            .split_at_mut(src_archetype.max(dst_archetype) as usize);

        let (src, dst) = match src_archetype < dst_archetype {
            true => (&mut before[src_archetype as usize], &mut after[0]),
            false => (&mut after[0], &mut before[dst_archetype as usize]),
        };

        let (dst_idx, opt_src_id) = unsafe { src.drop_bundle(entity, dst, idx, encoder) };

        self.entities
            .set_location(entity.idx(), dst_archetype, dst_idx);

        if let Some(src_id) = opt_src_id {
            self.entities.set_location(src_id, src_archetype, idx);
        }

        Ok(())
    }

    /// Inserts bundle of components to the specified entity.
    /// This is moral equivalent to calling `World::insert` with each component separately,
    /// but more efficient.
    ///
    /// For each component type in bundle:
    /// If entity already had component of that type,
    /// old component value is replaced with new one.
    /// Otherwise new component is added to the entity.
    ///
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn insert_bundle<B>(&mut self, entity: EntityId, bundle: B) -> Result<(), NoSuchEntity>
    where
        B: DynamicComponentBundle,
    {
        with_encoder!(self, encoder => self.insert_bundle_with_encoder(entity, bundle, &mut encoder))
    }

    #[inline]
    pub(crate) fn insert_bundle_with_encoder<B>(
        &mut self,
        entity: EntityId,
        bundle: B,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        B: DynamicComponentBundle,
    {
        self.insert_bundle_with_encoder_impl(entity, bundle, encoder, register_bundle::<B>)
    }

    /// Inserts bundle of components to the specified entity.
    /// This is moral equivalent to calling `World::insert` with each component separately,
    /// but more efficient.
    ///
    /// For each component type in bundle:
    /// If entity already had component of that type,
    /// old component value is replaced with new one.
    /// Otherwise new component is added to the entity.
    ///
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn insert_external_bundle<B>(
        &mut self,
        entity: EntityId,
        bundle: B,
    ) -> Result<(), NoSuchEntity>
    where
        B: DynamicBundle,
    {
        with_encoder!(self, encoder => self.insert_external_bundle_with_encoder(entity, bundle, &mut encoder))
    }

    #[inline]
    pub(crate) fn insert_external_bundle_with_encoder<B>(
        &mut self,
        entity: EntityId,
        bundle: B,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        B: DynamicBundle,
    {
        self.insert_bundle_with_encoder_impl(entity, bundle, encoder, assert_registered_bundle::<B>)
    }

    fn insert_bundle_with_encoder_impl<B, F>(
        &mut self,
        entity: EntityId,
        bundle: B,
        encoder: &mut ActionEncoder,
        register_bundle: F,
    ) -> Result<(), NoSuchEntity>
    where
        B: DynamicBundle,
        F: FnOnce(&mut ComponentRegistry, &B),
    {
        if !bundle.valid() {
            panic!(
                "Specified bundle `{}` is not valid. Check for duplicate component types",
                type_name::<B>()
            );
        }

        let (src_archetype, idx) = self.entities.get(entity).ok_or(NoSuchEntity)?;

        if bundle.with_ids(|ids| ids.is_empty()) {
            return Ok(());
        }

        self.epoch += 1;

        let dst_archetype = self.edges.insert_bundle(
            &mut self.registry,
            &mut self.archetypes,
            src_archetype,
            &bundle,
            |registry| register_bundle(registry, &bundle),
        );

        if dst_archetype == src_archetype {
            unsafe {
                self.archetypes[src_archetype as usize]
                    .set_bundle(entity, idx, bundle, self.epoch, encoder)
            };
            return Ok(());
        }

        let (before, after) = self
            .archetypes
            .split_at_mut(src_archetype.max(dst_archetype) as usize);

        let (src, dst) = match src_archetype < dst_archetype {
            true => (&mut before[src_archetype as usize], &mut after[0]),
            false => (&mut after[0], &mut before[dst_archetype as usize]),
        };

        let (dst_idx, opt_src_id) =
            unsafe { src.insert_bundle(entity, dst, idx, bundle, self.epoch, encoder) };

        self.entities
            .set_location(entity.idx(), dst_archetype, dst_idx);

        if let Some(src_id) = opt_src_id {
            self.entities.set_location(src_id, src_archetype, idx);
        }

        Ok(())
    }

    /// Drops components of the specified entity with type from the bundle.
    /// Skips any component type entity doesn't have.
    ///
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn drop_bundle<B>(&mut self, entity: EntityId) -> Result<(), NoSuchEntity>
    where
        B: Bundle,
    {
        with_encoder!(self, encoder => self.drop_bundle_with_encoder::<B>(entity, &mut encoder))
    }

    #[inline]
    pub(crate) fn drop_bundle_with_encoder<B>(
        &mut self,
        entity: EntityId,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        B: Bundle,
    {
        if !B::static_valid() {
            panic!(
                "Specified bundle `{}` is not valid. Check for duplicate component types",
                type_name::<B>()
            );
        }

        let (src_archetype, idx) = self.entities.get(entity).ok_or(NoSuchEntity)?;

        if B::static_with_ids(|ids| {
            ids.iter()
                .all(|&id| !self.archetypes[src_archetype as usize].contains_id(id))
        }) {
            // No components to remove.
            return Ok(());
        }

        self.epoch += 1;

        let dst_archetype = self
            .edges
            .remove_bundle::<B>(&mut self.archetypes, src_archetype);

        debug_assert_ne!(src_archetype, dst_archetype);

        let (before, after) = self
            .archetypes
            .split_at_mut(src_archetype.max(dst_archetype) as usize);

        let (src, dst) = match src_archetype < dst_archetype {
            true => (&mut before[src_archetype as usize], &mut after[0]),
            false => (&mut after[0], &mut before[dst_archetype as usize]),
        };

        let (dst_idx, opt_src_id) = unsafe { src.drop_bundle(entity, dst, idx, encoder) };

        self.entities
            .set_location(entity.idx(), dst_archetype, dst_idx);

        if let Some(src_id) = opt_src_id {
            self.entities.set_location(src_id, src_archetype, idx);
        }

        Ok(())
    }

    /// Adds relation between two entities to the [`World`]
    #[inline]
    pub fn add_relation<R>(
        &mut self,
        entity: EntityId,
        relation: R,
        target: EntityId,
    ) -> Result<(), NoSuchEntity>
    where
        R: Relation,
    {
        with_encoder!(self, encoder => self.add_relation_with_encoder(entity, relation, target, &mut encoder))
    }

    /// Adds relation between two entities to the [`World`]
    pub fn add_relation_with_encoder<R>(
        &mut self,
        entity: EntityId,
        relation: R,
        target: EntityId,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        R: Relation,
    {
        self.entities.get(entity).ok_or(NoSuchEntity)?;
        self.entities.get(target).ok_or(NoSuchEntity)?;

        self.epoch += 1;

        if R::SYMMETRIC {
            insert_component(
                self,
                entity,
                relation,
                encoder,
                |relation| OriginComponent::new(target, relation),
                |component, relation, encoder| component.add(entity, target, relation, encoder),
            );

            if target != entity {
                insert_component(
                    self,
                    target,
                    relation,
                    encoder,
                    |relation| OriginComponent::new(entity, relation),
                    |component, relation, encoder| component.add(target, entity, relation, encoder),
                );
            }
        } else {
            insert_component(
                self,
                entity,
                relation,
                encoder,
                |relation| OriginComponent::new(target, relation),
                |component, relation, encoder| component.add(entity, target, relation, encoder),
            );

            insert_component(
                self,
                target,
                (),
                encoder,
                |()| TargetComponent::<R>::new(entity),
                |component, (), _| component.add(entity),
            );
        }
        Ok(())
    }

    /// Adds relation between two entities to the [`World`]
    #[inline]
    pub fn drop_relation<R>(
        &mut self,
        entity: EntityId,
        target: EntityId,
    ) -> Result<(), NoSuchEntity>
    where
        R: Relation,
    {
        with_encoder!(self, encoder => self.drop_relation_with_encoder::<R>(entity, target, &mut encoder))
    }

    /// Adds relation between two entities to the [`World`]
    pub fn drop_relation_with_encoder<R>(
        &mut self,
        entity: EntityId,
        target: EntityId,
        encoder: &mut ActionEncoder,
    ) -> Result<(), NoSuchEntity>
    where
        R: Relation,
    {
        self.entities.get(entity).ok_or(NoSuchEntity)?;
        self.entities.get(target).ok_or(NoSuchEntity)?;

        if let Ok(c) = self.query_one_mut::<&mut OriginComponent<R>>(entity) {
            c.remove(entity, target, encoder);
        }

        Ok(())
    }

    /// Queries components from specified entity.
    ///
    /// If query cannot be satisfied, returns `QueryOneError::NotSatisfied`.
    #[inline]
    pub fn query_one<'a, Q>(
        &'a self,
        entity: EntityId,
    ) -> Result<PhantomQueryItem<'a, Q>, QueryOneError>
    where
        Q: PhantomQuery,
    {
        self.query_one_state(entity, PhantomData::<Q>)
    }

    /// Queries components from specified entity.
    ///
    /// If query cannot be satisfied, returns `QueryOneError::NotSatisfied`.
    #[inline]
    pub fn query_one_state<'a, Q>(
        &'a self,
        entity: EntityId,
        mut query: Q,
    ) -> Result<QueryItem<'a, Q>, QueryOneError>
    where
        Q: Query,
    {
        assert!(query.is_valid(), "Invalid query specified");

        let (archetype, idx) = self
            .entities
            .get(entity)
            .ok_or(QueryOneError::NoSuchEntity)?;

        let archetype = &self.archetypes[archetype as usize];

        debug_assert!(archetype.len() >= idx as usize, "Entity index is valid");

        if query.skip_archetype(archetype) {
            return Err(QueryOneError::NotSatisfied);
        }

        let mut fetch = unsafe { query.fetch(archetype, self.epoch) };

        if unsafe { fetch.skip_chunk(chunk_idx(idx as usize)) } {
            return Err(QueryOneError::NotSatisfied);
        }

        unsafe { fetch.visit_chunk(chunk_idx(idx as usize)) }

        if unsafe { fetch.skip_item(idx as usize) } {
            return Err(QueryOneError::NotSatisfied);
        }

        let item = unsafe { fetch.get_item(idx as usize) };
        Ok(item)
    }

    /// Queries components from specified entity.
    ///
    /// If query cannot be satisfied, returns `EntityError::MissingComponents`.
    #[inline]
    pub fn query_one_mut<'a, Q>(
        &'a mut self,
        entity: EntityId,
    ) -> Result<PhantomQueryItem<'a, Q>, QueryOneError>
    where
        Q: PhantomQuery,
    {
        self.query_one_state_mut(entity, PhantomData::<Q>)
    }

    /// Queries components from specified entity.
    ///
    /// If query cannot be satisfied, returns `EntityError::MissingComponents`.
    #[inline]
    pub fn query_one_state_mut<'a, Q>(
        &'a mut self,
        entity: EntityId,
        mut query: Q,
    ) -> Result<QueryItem<'a, Q>, QueryOneError>
    where
        Q: Query,
    {
        assert!(query.is_valid(), "Invalid query specified");

        self.epoch += 1;

        let (archetype, idx) = self
            .entities
            .get(entity)
            .ok_or(QueryOneError::NoSuchEntity)?;

        let archetype = &self.archetypes[archetype as usize];

        debug_assert!(archetype.len() >= idx as usize, "Entity index is valid");

        if query.skip_archetype(archetype) {
            return Err(QueryOneError::NotSatisfied);
        }

        let mut fetch = unsafe { query.fetch(archetype, self.epoch) };

        if unsafe { fetch.skip_chunk(chunk_idx(idx as usize)) } {
            return Err(QueryOneError::NotSatisfied);
        }

        unsafe { fetch.visit_chunk(chunk_idx(idx as usize)) }

        if unsafe { fetch.skip_item(idx as usize) } {
            return Err(QueryOneError::NotSatisfied);
        }

        let item = unsafe { fetch.get_item(idx as usize) };
        Ok(item)
    }

    /// Returns new [`Tracks`] instance to use with tracking queries.
    ///
    /// Returned [`Tracks`] instance considers only modifications
    /// that happen after this function call as "new" for the first tracking query.
    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Attemtps to check if specified entity has componet of specified type.
    ///
    /// If entity is not alive, fails with `Err(NoSuchEntity)`.
    #[inline]
    pub fn has_component<T: 'static>(&self, entity: EntityId) -> Result<bool, NoSuchEntity> {
        let (archetype, _idx) = self.entities.get(entity).ok_or(NoSuchEntity)?;
        Ok(self.archetypes[archetype as usize].contains_id(TypeId::of::<T>()))
    }

    /// Checks if specified entity is still alive.
    #[inline]
    pub fn is_alive(&self, entity: EntityId) -> bool {
        self.entities.get(entity).is_some()
    }

    /// Queries the world to iterate over entities and components specified by the query type.
    ///
    /// This method only works with immutable queries.
    #[inline]
    pub fn query<'a, Q>(&'a self) -> QueryRef<'a, (PhantomData<Q>,), ()>
    where
        Q: PhantomQuery,
    {
        self.make_query(PhantomData)
    }

    /// Queries the world to iterate over entities and components specified by the query type.
    ///
    /// This method only works with immutable queries.
    #[inline]
    pub fn make_query<'a, Q>(&'a self, query: Q) -> QueryRef<'a, (Q,), ()>
    where
        Q: Query,
    {
        assert!(query.is_valid(), "Invalid query specified");

        QueryRef::new(&self.archetypes, self.epoch, (query,), ())
    }

    /// Queries the world to iterate over entities and components specified by the query type.
    ///
    /// This method can be used for queries that mutate components.
    #[inline]
    pub fn query_mut<'a, Q>(&'a mut self) -> QueryMut<'a, (PhantomData<Q>,), ()>
    where
        Q: PhantomQuery,
    {
        self.make_query_mut(PhantomData)
    }

    /// Queries the world to iterate over entities and components specified by the query type.
    ///
    /// This method can be used for queries that mutate components.
    #[inline]
    pub fn make_query_mut<'a, Q>(&'a mut self, query: Q) -> QueryMut<'a, (Q,), ()>
    where
        Q: Query,
    {
        assert!(query.is_valid(), "Invalid query specified");
        self.build_query_mut().extend_query(query)
    }

    /// Starts building immutable query.
    #[inline]
    pub fn build_query<'a>(&'a self) -> QueryRef<'a, (), ()> {
        QueryRef::new(&self.archetypes, self.epoch, (), ())
    }

    /// Queries the world to iterate over entities and components specified by the query type.
    ///
    /// This method only works with immutable queries.
    #[inline]
    pub fn build_query_mut<'a>(&'a mut self) -> QueryMut<'a, (), ()> {
        QueryMut::new(&self.archetypes, &mut self.epoch, (), ())
    }

    /// Splits the world into entity-meta and mutable query.
    /// Queries the world to iterate over entities and components specified by the query type.
    /// `EntityMeta` can be used to fetch and control some meta-information about entities while query is alive,
    /// including checking if entity is alive, checking components attached to entity and taking, giving entity ownership.
    ///
    /// This method can be used for queries that mutate components.
    #[inline]
    pub fn meta_query_mut<'a, Q>(&'a mut self, query: Q) -> (EntityMeta<'a>, QueryMut<'a, (Q,), ()>)
    where
        Q: Query,
    {
        assert!(query.is_valid(), "Invalid query specified");

        let meta = EntityMeta {
            entities: &mut self.entities,
            archetypes: &self.archetypes,
        };
        let query = QueryMut::new(&self.archetypes, &mut self.epoch, (query,), ());
        (meta, query)
    }
}

/// Spawning iterator. Produced by [`World::spawn_batch`].
#[allow(missing_debug_implementations)]
pub struct SpawnBatch<'a, I> {
    bundles: I,
    epoch: u64,
    archetype_idx: u32,
    archetype: &'a mut Archetype,
    entities: &'a mut Entities,
}

impl<B, I> SpawnBatch<'_, I>
where
    I: Iterator<Item = B>,
    B: Bundle,
{
    /// Spawns the rest of the entities, dropping their ids.
    ///
    /// Note that `SpawnBatchOwned` does not have this methods
    /// as dropped `Entity` references would cause spawned entities
    /// to be despawned, and that's probably not what user wants.
    pub fn spawn_all(mut self) {
        spawn_reserve(&self.bundles, self.archetype);

        let entities = &mut self.entities;
        let archetype = &mut self.archetype;
        let archetype_idx = self.archetype_idx;
        let epoch = self.epoch;

        self.bundles.for_each(|bundle| {
            let entity = entities.spawn();
            let idx = archetype.spawn(entity, bundle, epoch);
            entities.set_location(entity.idx(), archetype_idx, idx);
        })
    }
}

impl<B, I> Iterator for SpawnBatch<'_, I>
where
    I: Iterator<Item = B>,
    B: Bundle,
{
    type Item = EntityId;

    fn next(&mut self) -> Option<EntityId> {
        let bundle = self.bundles.next()?;

        let entity = self.entities.spawn();
        let idx = self.archetype.spawn(entity, bundle, self.epoch);

        self.entities
            .set_location(entity.idx(), self.archetype_idx, idx);

        Some(entity)
    }

    fn nth(&mut self, n: usize) -> Option<EntityId> {
        // No reason to create entities
        // for which the only reference is immediatelly dropped
        let bundle = self.bundles.nth(n)?;

        let entity = self.entities.spawn();
        let idx = self.archetype.spawn(entity, bundle, self.epoch);

        self.entities
            .set_location(entity.idx(), self.archetype_idx, idx);

        Some(entity)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.bundles.size_hint()
    }

    fn fold<T, F>(mut self, init: T, mut f: F) -> T
    where
        F: FnMut(T, EntityId) -> T,
    {
        spawn_reserve(&self.bundles, self.archetype);

        let entities = &mut self.entities;
        let archetype = &mut self.archetype;
        let archetype_idx = self.archetype_idx;
        let epoch = self.epoch;

        self.bundles.fold(init, |acc, bundle| {
            let entity = entities.spawn();
            let idx = archetype.spawn(entity, bundle, epoch);
            entities.set_location(entity.idx(), archetype_idx, idx);
            f(acc, entity)
        })
    }

    fn collect<T>(self) -> T
    where
        T: FromIterator<EntityId>,
    {
        // `FromIterator::from_iter` would probably just call `fn next()`
        // until the end of the iterator.
        //
        // Hence we should reserve space in archetype here.
        spawn_reserve(&self.bundles, self.archetype);

        FromIterator::from_iter(self)
    }
}

impl<B, I> ExactSizeIterator for SpawnBatch<'_, I>
where
    I: ExactSizeIterator<Item = B>,
    B: Bundle,
{
    fn len(&self) -> usize {
        self.bundles.len()
    }
}

impl<B, I> DoubleEndedIterator for SpawnBatch<'_, I>
where
    I: DoubleEndedIterator<Item = B>,
    B: Bundle,
{
    fn next_back(&mut self) -> Option<EntityId> {
        let bundle = self.bundles.next_back()?;

        let entity = self.entities.spawn();
        let idx = self.archetype.spawn(entity, bundle, self.epoch);

        self.entities
            .set_location(entity.idx(), self.archetype_idx, idx);

        Some(entity)
    }

    fn nth_back(&mut self, n: usize) -> Option<EntityId> {
        // No reason to create entities
        // for which the only reference is immediatelly dropped
        let bundle = self.bundles.nth_back(n)?;

        let entity = self.entities.spawn();
        let idx = self.archetype.spawn(entity, bundle, self.epoch);

        self.entities
            .set_location(entity.idx(), self.archetype_idx, idx);

        Some(entity)
    }

    fn rfold<T, F>(mut self, init: T, mut f: F) -> T
    where
        Self: Sized,
        F: FnMut(T, EntityId) -> T,
    {
        spawn_reserve(&self.bundles, self.archetype);

        let entities = &mut self.entities;
        let archetype = &mut self.archetype;
        let archetype_idx = self.archetype_idx;
        let epoch = self.epoch;

        self.bundles.rfold(init, |acc, bundle| {
            let entity = entities.spawn();
            let idx = archetype.spawn(entity, bundle, epoch);
            entities.set_location(entity.idx(), archetype_idx, idx);
            f(acc, entity)
        })
    }
}

impl<B, I> FusedIterator for SpawnBatch<'_, I>
where
    I: FusedIterator<Item = B>,
    B: Bundle,
{
}

/// Error returned in case specified [`EntityId`]
/// does not reference any live entity in the [`World`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoSuchEntity;

impl fmt::Display for NoSuchEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Speicified entity is not found")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for NoSuchEntity {}

/// Error returned in case specified entity does not contain
/// component of required type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MissingComponents;

impl fmt::Display for MissingComponents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Speicified component is not found in entity")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MissingComponents {}

/// Error returned if either entity reference is invalid
/// or component of required type is not found for an entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityError {
    /// Error returned in case specified [`EntityId`]
    /// does not reference any live entity in the [`World`].
    NoSuchEntity,

    /// Error returned in case specified entity does not contain
    /// component of required type.
    MissingComponents,
}

impl fmt::Display for EntityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchEntity => fmt::Display::fmt(&NoSuchEntity, f),
            Self::MissingComponents => fmt::Display::fmt(&MissingComponents, f),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EntityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoSuchEntity => Some(&NoSuchEntity),
            Self::MissingComponents => Some(&MissingComponents),
        }
    }
}

impl From<NoSuchEntity> for EntityError {
    fn from(_: NoSuchEntity) -> Self {
        EntityError::NoSuchEntity
    }
}

impl From<MissingComponents> for EntityError {
    fn from(_: MissingComponents) -> Self {
        EntityError::MissingComponents
    }
}

impl PartialEq<NoSuchEntity> for EntityError {
    fn eq(&self, _: &NoSuchEntity) -> bool {
        matches!(self, EntityError::NoSuchEntity)
    }
}

impl PartialEq<MissingComponents> for EntityError {
    fn eq(&self, _: &MissingComponents) -> bool {
        matches!(self, EntityError::MissingComponents)
    }
}

/// Error returned by [`query_one_*`] method family
/// when query is not satisfied by the entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryOneError {
    /// Error returned in case specified [`EntityId`]
    /// does not reference any live entity in the [`World`].
    NoSuchEntity,

    /// Error returned in case specified entity does not contain
    /// component of required type.
    NotSatisfied,
}

impl fmt::Display for QueryOneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchEntity => fmt::Display::fmt(&NoSuchEntity, f),
            Self::NotSatisfied => f.write_str("Query is not satisfied"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for QueryOneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoSuchEntity => Some(&NoSuchEntity),
            Self::NotSatisfied => None,
        }
    }
}

impl From<NoSuchEntity> for QueryOneError {
    fn from(_: NoSuchEntity) -> Self {
        QueryOneError::NoSuchEntity
    }
}

impl PartialEq<NoSuchEntity> for QueryOneError {
    fn eq(&self, _: &NoSuchEntity) -> bool {
        matches!(self, QueryOneError::NoSuchEntity)
    }
}

/// Inserts component.
/// This function uses different code to assign component when it already exists on entity.
fn insert_component<T, C>(
    world: &mut World,
    entity: EntityId,
    value: T,
    encoder: &mut ActionEncoder,
    into_component: impl FnOnce(T) -> C,
    set_component: impl FnOnce(&mut C, T, &mut ActionEncoder),
) where
    C: Component,
{
    let (src_archetype, idx) = world.entities.get(entity).unwrap();

    if world.archetypes[src_archetype as usize].contains_id(TypeId::of::<C>()) {
        let component =
            unsafe { world.archetypes[src_archetype as usize].get_mut::<C>(idx, world.epoch) };

        set_component(component, value, encoder);

        return;
    }

    let component = into_component(value);

    let dst_archetype = world.edges.insert(
        TypeId::of::<C>(),
        &mut world.registry,
        &mut world.archetypes,
        src_archetype,
        |registry| registry.get_or_register::<C>(),
    );

    debug_assert_ne!(src_archetype, dst_archetype);

    let (before, after) = world
        .archetypes
        .split_at_mut(src_archetype.max(dst_archetype) as usize);

    let (src, dst) = match src_archetype < dst_archetype {
        true => (&mut before[src_archetype as usize], &mut after[0]),
        false => (&mut after[0], &mut before[dst_archetype as usize]),
    };

    let (dst_idx, opt_src_id) = unsafe { src.insert(entity, dst, idx, component, world.epoch) };

    world
        .entities
        .set_location(entity.idx(), dst_archetype, dst_idx);

    if let Some(src_id) = opt_src_id {
        world.entities.set_location(src_id, src_archetype, idx);
    }
}

fn register_one<T: Component>(registry: &mut ComponentRegistry) -> &ComponentInfo {
    registry.get_or_register::<T>()
}

fn assert_registered_one<T: 'static>(registry: &mut ComponentRegistry) -> &ComponentInfo {
    match registry.get_info(TypeId::of::<T>()) {
        Some(info) => info,
        None => panic!(
            "Component {}({:?}) is not registered",
            type_name::<T>(),
            TypeId::of::<T>()
        ),
    }
}

fn register_bundle<B: ComponentBundleDesc>(registry: &mut ComponentRegistry, bundle: &B) {
    bundle.with_components(|infos| {
        for info in infos {
            registry.get_or_register_raw(info.clone());
        }
    });
}

fn assert_registered_bundle<B: BundleDesc>(registry: &mut ComponentRegistry, bundle: &B) {
    bundle.with_ids(|ids| {
        for (idx, id) in ids.iter().enumerate() {
            if registry.get_info(*id).is_none() {
                panic!(
                    "Component {:?} - ({}[{}]) is not registered",
                    id,
                    type_name::<B>(),
                    idx
                );
            }
        }
    })
}
