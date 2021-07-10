use std::ptr::{self, NonNull};
use std::marker::PhantomData;
use std::cell::UnsafeCell;
use std::rc::Rc;
use std::ops::{Deref, DerefMut};

pub struct Visitor {
    _not_pub_constructable: (),
}

pub trait Trace {
    fn trace(&self, visitor: &Visitor);
}

pub trait Sealed {}

pub trait Traverse: Sealed {
    fn mark(&mut self);
    fn unmark(&mut self);
    fn marked(&self) -> bool;
    fn next(&self) -> GcPtr;
    fn prev(&self) -> GcPtr;
}

pub type GcPtr = *const dyn Traverse;
pub type GcPtrNonNull = NonNull<dyn Traverse>;

pub struct Arena {
    start: GcPtr,
    roots: Vec<GcPtrNonNull>,
}

pub struct ArenaIter<'a> {
    cur: GcPtr,
    // Mark as referring to the Arena, even though we just chase internal pointers.
    // The &mut is because concurrent access is not yet safe.
    marker: PhantomData<&'a mut Arena>,
}

// As with RcBox, repr(C) forces field order (to be sure that the layout is compatible with T:
// ?Sized).
#[repr(C)]
pub struct GcBox<T: ?Sized> {
    mark: bool,
    next: GcPtr,
    prev: GcPtr,
    meta: *const (),  // vtable for &T as &dyn Trace
    alloc: *mut GcAlloc<T>,  // point back to our alloc for dropping
    value: T,
}

pub struct GcAlloc<T: ?Sized> {
    inner: Option<NonNull<GcBox<T>>>,
}

pub struct Gc<T: ?Sized> {
    ptr: Rc<UnsafeCell<GcAlloc<T>>>,
    marker: PhantomData<GcBox<T>>,
}

pub struct Collection {
    pub total: usize,
    pub collected: usize,
}

fn null_gcptr() -> GcPtr {
    unsafe {
        // SAFETY: very little.
        // The layout of this double matches the (unstable) core::raw::TraitObject. Beyond that,
        // anything goes.
        std::mem::transmute(
            (ptr::null::<*const ()>(), ptr::null::<*const ()>())
        )
    }
}

fn extract_meta(t: &dyn Trace) -> *const () {
    unsafe {
        // SAFETY: as above.
        // XXX Cast might be superfluous.
        let (_ptr, meta): (*const (), *const ()) =
            std::mem::transmute(t as *const dyn Trace);
        meta
    }
}

impl Arena {
    pub fn new() -> Self {
        Self {
            start: null_gcptr(),
            roots: Vec::new(),
        }
    }

    pub fn gc<T: Trace + 'static>(&mut self, value: T) -> Gc<T> {
        let gc = Gc::new(value);
        unsafe {
            // SAFETY: We're confident that this freshly-constructed Gc contains a unique, new
            // allocation (by Box) to a GcBox.
            let pt = (*gc.ptr.get()).inner.unwrap().as_ptr();
            if let Some(gcbox) = (self.start as *mut GcBox<()>).as_mut() {
                gcbox.prev = pt;
            }
            (*gc.ptr.get()).inner.unwrap().as_mut().next = self.start;
            self.start = pt;
        }
        gc
    }
    
    pub fn root<T: Trace + 'static>(&mut self, value: T) -> Gc<T> {
        let gc = self.gc(value);
        unsafe {
            // SAFETY: Most of the worry here is just dereferencing the UnsafeCell. Since the
            // interior type is Copy, this should be fine.
            self.roots.push((*gc.ptr.get()).inner.unwrap());
        }
        gc
    }

    pub fn make_root<T: 'static>(&mut self, gc: &Gc<T>) {
        let inner = unsafe {
            // SAFETY: as above, this is mostly because we're using an UnsafeCell. No ref is coined
            // here, so we're not making any promises about lifetimes we can't keep.
            (*gc.ptr.get()).inner
        };
        // FIXME: this is still slow
        if let Some(inner) = inner {  // NB the shadow
            if ! self.roots.iter().any(|p| {
                std::ptr::eq(p.as_ptr(), inner.as_ptr())
            }) {  // Avoid duplicates in the roots
                self.roots.push(inner);
            }
        }
    }

    pub fn unroot<T>(&mut self, gc: &Gc<T>) {
        // FIXME: This is expected to be a cold path
        self.roots = self.roots.iter().cloned()
            .filter(|ptr| !ptr::eq(ptr.as_ptr() as *const _, unsafe { *gc.ptr.get() }.inner.unwrap().as_ptr() as *const _))
            .collect();
    }

    pub fn iter<'s>(&'s mut self) -> ArenaIter<'s> {
        ArenaIter {
            cur: self.start,
            marker: PhantomData,
        }
    }

    pub fn collect(&mut self) -> Collection {
        let mut col = Collection {
            total: 0, collected: 0,
        };
        for mut t in self.iter() {
            unsafe {
                // SAFETY: We expect these to have been already constructed and aligned normally,
                // and this iterator--strictly speaking--returns only non-null pointers.
                t.as_mut().unmark();
            }
            col.total += 1;
        }
        let visitor = Visitor {
            _not_pub_constructable: (),
        };
        // Strictly speaking, we don't mutate the _values_ in this list, but we do mutate their
        // referents through the underlying raw pointer.
        for r in &mut self.roots {
            unsafe {
                // SAFETY: By virtue of this very line, the roots list cannot be left with dangling
                // pointers (as all member objects are marked).
                (*r).as_mut().mark();
            }
            // With that mut borrow out of scope, do the recursive trace
            unsafe {
                // SAFETY: Begin your dragon prayers.
                // We've sealed Traverse as a trait, so we know our implementors (and it's only
                // GcBox).
                // This cast intentionally discards the Traverse vtable--we won't need it again.
                // The type we chose for T is definitely wrong, but we make up for it by manually
                // assembling the fat pointer.
                let gcbox = r.cast::<GcBox<()>>().as_ref();
                let tobj: &dyn Trace = std::mem::transmute(
                    (&gcbox.value, gcbox.meta)
                );
                tobj.trace(&visitor);
            }
        }
        let mut start = self.start;
        for t in self.iter() {
            if ! unsafe { t.as_ref().marked() } {
                unsafe {
                    let boxptr = t.cast::<GcBox<()>>();
                    // TODO XXX: Unthread pointers in gcbox and change self.start if need be
                    if ptr::eq(start, t.as_ptr()) {
                        start = boxptr.as_ref().next;
                    }
                    {
                        let boxmut = boxptr.as_ptr().as_mut().unwrap();
                        if let Some(prevbox) = (boxmut.prev as *mut GcBox<()>).as_mut() {
                            prevbox.next = boxmut.next;
                        }
                        if let Some(nextbox) = (boxmut.next as *mut GcBox<()>).as_mut() {
                            nextbox.prev = boxmut.prev;
                        }
                    }
                    // Null out the pointer to the box from its alloc, so all the Gc<T>'s pointing
                    // here know that the allocation is gone.
                    (*t.cast::<GcBox<()>>().as_mut().alloc).inner = None;
                    // Collect the box again and let it drop
                    Box::from_raw(t.as_ptr());
                }
                col.collected += 1;
            }
        }
        self.start = start;
        col
    }
}

impl<'s> IntoIterator for &'s mut Arena {
    type Item = GcPtrNonNull;
    type IntoIter = ArenaIter<'s>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

// Note the use of associated methods because of Deref<Target=T>.
impl<T: ?Sized> Gc<T> {
    pub fn try_as_ref(this: &Self) -> Option<&T> {
        unsafe {
            // SAFETY: we're bounding the reference implicitly with the lifetime on self.
            (*this.ptr.get()).inner.map(|pr| {
                &pr.as_ref().value
            })
        }
    }

    pub fn as_ref(this: &Self) -> &T {
        Self::try_as_ref(this).expect("Gc::as_ref on collected object")
    }

    pub fn try_as_mut(this: &mut Self) -> Option<&mut T> {
        unsafe {
            // SAFETY: As above; note the mutable borrow of self to statically guarantee
            // uniqueness.
            (*this.ptr.get()).inner.map(|mut pr| {
                &mut pr.as_mut().value
            })
        }
    }

    pub fn as_mut(this: &mut Self) -> &mut T {
        Self::try_as_mut(this).expect("Gc::as_mut on collected object")
    }
}

impl<T: Trace> Gc<T> {
    fn new(value: T) -> Self {
        let result = Self {
            ptr: Rc::new(UnsafeCell::new(GcAlloc {
                inner: Some(Box::leak(
                         Box::new(GcBox {
                             mark: false,
                             next: null_gcptr(),
                             prev: null_gcptr(),
                             meta: extract_meta(&value as &dyn Trace),
                             alloc: ptr::null_mut(),
                             value
                         })
                    ).into()),
                })),
            marker: PhantomData,
        };
        unsafe {
            // SAFETY: result.ptr is an UnsafeCell, so the compiler knows this can alias. In
            // addtion, only one reference (a mutable one) is coined here, and dropped by this
            // block--the provnenance doesn't include the *mut coming out of UnsafeCell::get().
            (*result.ptr.get()).inner.unwrap().as_mut().alloc =
                result.ptr.get();
        }
        result
    }
}

impl<T> Clone for Gc<T> {
    fn clone(&self) -> Self {
        Self {
            ptr: Rc::clone(&self.ptr),
            marker: PhantomData,
        }
    }
}

impl<T> Deref for Gc<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target { Gc::as_ref(self) }
}

impl<T> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target { Gc::as_mut(self) }
}

impl<T: ?Sized> Sealed for GcBox<T> {}
impl<T: ?Sized> Traverse for GcBox<T> {
    fn mark(&mut self) { self.mark = true; }
    fn unmark(&mut self) { self.mark = false; }
    fn marked(&self) -> bool { self.mark }
    fn next(&self) -> GcPtr { self.next }
    fn prev(&self) -> GcPtr { self.prev }
}

impl<'a> Iterator for ArenaIter<'a> {
    type Item = GcPtrNonNull;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            // SAFETY: Pray to the dragons that we've maintained a valid linked list elsewhere
            self.cur.as_ref()
        }.map(|t| {
            self.cur = t.next();
            unsafe {
                // SAFETY: This code is reachable only when the pointer isn't null
                GcPtrNonNull::new_unchecked(t as GcPtr as *mut _)
            }
        })
    }
}

// Manual impls to avoid constraints on the underlying T
impl<T: ?Sized> Clone for GcAlloc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for GcAlloc<T> {}

impl Visitor {
    pub fn visit<T>(&self, gc: &Gc<T>) {
        let gcbox = unsafe {
            // SAFETY: Rely on this being constructed and not dropped.
            // Aliasing: Rust would normally complain about seeking a &mut from our &Gc, but the
            // underlying raw pointer is represented as mutable.
            (*gc.ptr.get()).inner.unwrap().as_mut()
        };
        if gcbox.mark {
            return;
        }
        gcbox.mark = true;
        unsafe {
            // SAFETY: See Arena::collect.
            let tobj: &dyn Trace = std::mem::transmute(
                (&gcbox.value, gcbox.meta)
            );
            tobj.trace(&self);
        }
    }
}

#[cfg(test)]
mod test;
