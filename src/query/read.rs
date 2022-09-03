use core::{any::TypeId, marker::PhantomData, ptr::NonNull};

use atomicell::{borrow::AtomicBorrow, Ref};

use crate::{archetype::Archetype, epoch::EpochId};

use super::{
    phantom::PhantomQuery, Access, Fetch, ImmutablePhantomQuery, ImmutableQuery, IntoQuery,
    PhantomQueryFetch,
};

/// `Fetch` type for the `&T` query.

pub struct FetchRead<'a, T> {
    ptr: NonNull<T>,
    _borrow: AtomicBorrow<'a>,
    marker: PhantomData<&'a [T]>,
}

unsafe impl<'a, T> Fetch<'a> for FetchRead<'a, T>
where
    T: Sync + 'a,
{
    type Item = &'a T;

    #[inline]
    fn dangling() -> Self {
        FetchRead {
            ptr: NonNull::dangling(),
            _borrow: AtomicBorrow::dummy(),
            marker: PhantomData,
        }
    }

    #[inline]
    unsafe fn skip_chunk(&mut self, _: usize) -> bool {
        false
    }

    #[inline]
    unsafe fn skip_item(&mut self, _: usize) -> bool {
        false
    }

    #[inline]
    unsafe fn visit_chunk(&mut self, _: usize) {}

    #[inline]
    unsafe fn get_item(&mut self, idx: usize) -> &'a T {
        &*self.ptr.as_ptr().add(idx)
    }
}

impl<T> IntoQuery for &T
where
    T: Sync + 'static,
{
    type Query = PhantomData<fn() -> Self>;
}

impl<'a, T> PhantomQueryFetch<'a> for &T
where
    T: Sync + 'static,
{
    type Item = &'a T;
    type Fetch = FetchRead<'a, T>;
}

impl<T> PhantomQuery for &T
where
    T: Sync + 'static,
{
    #[inline]
    fn access(ty: TypeId) -> Option<Access> {
        if ty == TypeId::of::<T>() {
            Some(Access::Read)
        } else {
            None
        }
    }

    #[inline]
    fn skip_archetype(archetype: &Archetype) -> bool {
        !archetype.contains_id(TypeId::of::<T>())
    }

    #[inline]
    unsafe fn fetch<'a>(archetype: &'a Archetype, _epoch: EpochId) -> FetchRead<'a, T> {
        let idx = archetype.id_index(TypeId::of::<T>()).unwrap_unchecked();
        let component = archetype.component(idx);
        debug_assert_eq!(component.id(), TypeId::of::<T>());

        let (data, borrow) = Ref::into_split(component.data.borrow());

        FetchRead {
            ptr: data.ptr.cast(),
            _borrow: borrow,
            marker: PhantomData,
        }
    }
}

unsafe impl<T> ImmutablePhantomQuery for &T where T: Sync + 'static {}

/// Returns query that yields reference to specified component
/// for each entity that has that component.
///
/// Skips entities that don't have the component.
pub fn read<T>() -> PhantomData<&'static T>
where
    T: Sync,
    for<'a> PhantomData<&'a T>: ImmutableQuery,
{
    PhantomData
}
