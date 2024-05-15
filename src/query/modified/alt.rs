use core::{any::TypeId, cell::Cell, marker::PhantomData, ptr::NonNull};

use crate::{
    archetype::{chunk_idx, Archetype},
    component::ComponentInfo,
    epoch::EpochId,
    query::{
        alt::{Alt, RefMut},
        option::OptionQuery,
        Access, AsQuery, Fetch, IntoQuery, Query, SendQuery, WriteAlias,
    },
    system::QueryArg,
    type_id,
};

use super::Modified;

/// [`Fetch`] type for the [`Modified<Alt<T>>`] query.
pub struct ModifiedFetchAlt<'a, T> {
    after_epoch: EpochId,
    epoch: EpochId,
    ptr: NonNull<T>,
    entity_epochs: NonNull<EpochId>,
    chunk_epochs: NonNull<Cell<EpochId>>,
    archetype_epoch: NonNull<Cell<EpochId>>,
    marker: PhantomData<&'a mut [T]>,
}

unsafe impl<'a, T> Fetch<'a> for ModifiedFetchAlt<'a, T>
where
    T: 'a,
{
    type Item = RefMut<'a, T>;

    #[inline(always)]
    fn dangling() -> Self {
        ModifiedFetchAlt {
            after_epoch: EpochId::start(),
            epoch: EpochId::start(),
            ptr: NonNull::dangling(),
            entity_epochs: NonNull::dangling(),
            chunk_epochs: NonNull::dangling(),
            archetype_epoch: NonNull::dangling(),
            marker: PhantomData,
        }
    }

    #[inline(always)]
    unsafe fn visit_chunk(&mut self, chunk_idx: u32) -> bool {
        let epoch = &*self.chunk_epochs.as_ptr().add(chunk_idx as usize);
        epoch.get().after(self.after_epoch)
    }

    #[inline(always)]
    unsafe fn visit_item(&mut self, idx: u32) -> bool {
        let epoch = *self.entity_epochs.as_ptr().add(idx as usize);
        epoch.after(self.after_epoch)
    }

    #[inline(always)]
    unsafe fn get_item(&mut self, idx: u32) -> RefMut<'a, T> {
        let archetype_epoch = &mut *self.archetype_epoch.as_ptr();
        let chunk_epoch = &mut *self.chunk_epochs.as_ptr().add(chunk_idx(idx) as usize);
        let entity_epoch = &mut *self.entity_epochs.as_ptr().add(idx as usize);

        debug_assert!(entity_epoch.before(self.epoch));

        RefMut {
            component: &mut *self.ptr.as_ptr().add(idx as usize),
            entity_epoch,
            chunk_epoch,
            archetype_epoch,
            epoch: self.epoch,
        }
    }
}

impl<T> AsQuery for Modified<Alt<T>>
where
    T: 'static,
{
    type Query = Self;
}

impl<T> IntoQuery for Modified<Alt<T>>
where
    T: 'static,
{
    #[inline(always)]
    fn into_query(self) -> Self::Query {
        self
    }
}

impl<T> QueryArg for Modified<Alt<T>>
where
    T: Send + 'static,
{
    #[inline(always)]
    fn new() -> Self {
        Modified {
            after_epoch: EpochId::start(),
            query: Alt,
        }
    }

    #[inline(always)]
    fn after(&mut self, world: &crate::world::World) {
        self.after_epoch = world.epoch();
    }
}

unsafe impl<T> Query for Modified<Alt<T>>
where
    T: 'static,
{
    type Item<'a> = RefMut<'a, T>;
    type Fetch<'a> = ModifiedFetchAlt<'a, T>;

    const MUTABLE: bool = true;

    #[inline(always)]
    fn component_access(&self, comp: &ComponentInfo) -> Result<Option<Access>, WriteAlias> {
        self.query.component_access(comp)
    }

    #[inline(always)]
    fn visit_archetype(&self, archetype: &Archetype) -> bool {
        match archetype.component(type_id::<T>()) {
            None => false,
            Some(component) => unsafe {
                debug_assert_eq!(self.query.visit_archetype(archetype), true);

                debug_assert_eq!(component.id(), type_id::<T>());
                let data = component.data_mut();
                data.epoch.after(self.after_epoch)
            },
        }
    }

    #[inline(always)]
    unsafe fn access_archetype(&self, _archetype: &Archetype, mut f: impl FnMut(TypeId, Access)) {
        f(type_id::<T>(), Access::Write)
    }

    #[inline(always)]
    unsafe fn fetch<'a>(
        &self,
        _arch_idx: u32,
        archetype: &'a Archetype,
        epoch: EpochId,
    ) -> ModifiedFetchAlt<'a, T> {
        let component = archetype.component(type_id::<T>()).unwrap_unchecked();
        debug_assert_eq!(component.id(), type_id::<T>());

        let data = component.data_mut();

        debug_assert!(data.epoch.after(self.after_epoch));
        debug_assert!(data.epoch.before(epoch));

        ModifiedFetchAlt {
            after_epoch: self.after_epoch,
            epoch,
            ptr: data.ptr.cast(),
            entity_epochs: NonNull::new_unchecked(data.entity_epochs.as_mut_ptr()),
            chunk_epochs: NonNull::new_unchecked(data.chunk_epochs.as_mut_ptr()).cast(),
            archetype_epoch: NonNull::from(&mut data.epoch).cast(),
            marker: PhantomData,
        }
    }
}

unsafe impl<T> SendQuery for Modified<Alt<T>> where T: Send + 'static {}

impl<T> AsQuery for Modified<Option<Alt<T>>>
where
    T: 'static,
{
    type Query = Modified<OptionQuery<Alt<T>>>;
}

impl<T> AsQuery for Modified<OptionQuery<Alt<T>>>
where
    T: 'static,
{
    type Query = Self;
}

impl<T> IntoQuery for Modified<OptionQuery<Alt<T>>>
where
    T: 'static,
{
    #[inline(always)]
    fn into_query(self) -> Self {
        self
    }
}

impl<T> QueryArg for Modified<OptionQuery<Alt<T>>>
where
    T: Send + 'static,
{
    #[inline(always)]
    fn new() -> Self {
        Modified {
            after_epoch: EpochId::start(),
            query: OptionQuery(Alt),
        }
    }

    #[inline(always)]
    fn after(&mut self, world: &crate::world::World) {
        self.after_epoch = world.epoch();
    }
}

unsafe impl<T> Query for Modified<OptionQuery<Alt<T>>>
where
    T: 'static,
{
    type Item<'a> = Option<RefMut<'a, T>>;
    type Fetch<'a> = Option<ModifiedFetchAlt<'a, T>>;

    const MUTABLE: bool = true;

    #[inline(always)]
    fn component_access(&self, comp: &ComponentInfo) -> Result<Option<Access>, WriteAlias> {
        self.query.component_access(comp)
    }

    #[inline(always)]
    fn visit_archetype(&self, archetype: &Archetype) -> bool {
        match archetype.component(type_id::<T>()) {
            None => true,
            Some(component) => unsafe {
                debug_assert_eq!(self.query.visit_archetype(archetype), true);

                debug_assert_eq!(component.id(), type_id::<T>());
                let data = component.data();
                data.epoch.after(self.after_epoch)
            },
        }
    }

    #[inline(always)]
    unsafe fn access_archetype(&self, archetype: &Archetype, mut f: impl FnMut(TypeId, Access)) {
        if let Some(component) = archetype.component(type_id::<T>()) {
            debug_assert_eq!(self.query.visit_archetype(archetype), true);

            debug_assert_eq!(component.id(), type_id::<T>());
            let data = component.data();
            if data.epoch.after(self.after_epoch) {
                f(type_id::<T>(), Access::Read)
            }
        }
    }

    #[inline(always)]
    unsafe fn fetch<'a>(
        &self,
        _arch_idx: u32,
        archetype: &'a Archetype,
        epoch: EpochId,
    ) -> Option<ModifiedFetchAlt<'a, T>> {
        match archetype.component(type_id::<T>()) {
            None => None,
            Some(component) => {
                let data = component.data_mut();

                debug_assert!(data.epoch.after(self.after_epoch));

                Some(ModifiedFetchAlt {
                    after_epoch: self.after_epoch,
                    epoch,
                    ptr: data.ptr.cast(),
                    entity_epochs: NonNull::new_unchecked(
                        data.entity_epochs.as_ptr() as *mut EpochId
                    ),
                    chunk_epochs: NonNull::new_unchecked(data.chunk_epochs.as_mut_ptr()).cast(),
                    archetype_epoch: NonNull::from(&mut data.epoch).cast(),
                    marker: PhantomData,
                })
            }
        }
    }
}

unsafe impl<T> SendQuery for Modified<OptionQuery<Alt<T>>> where T: Send + 'static {}
