use super::*;

use std::fmt::{self, Debug, Formatter};

// XXX don't try this at home; I'm doing this because (in the tests) I don't feel like introducing
// lifetime parameters.
pub struct RunOnDrop {
    func: fn(*mut ()),
    data: *mut (),
}

impl Drop for RunOnDrop {
    fn drop(&mut self) {
        (self.func)(self.data);
    }
}

enum Object {
    Simple,
    Container(Vec<Gc<Object>>),
    RunOnDrop(RunOnDrop),
}

// This implementation is intentionally minimal for the uses of these test cases.
impl PartialEq for Object {
    fn eq(&self, other: &Self) -> bool {
        use Object::*;
        match (self, other) {
            (Simple, Simple) => true,
            _ => false,
        }
    }
}

impl Eq for Object {}

impl Debug for Object {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        use Object::*;
        match self {
            Simple => write!(f, "Simple"),
            Container(v) => write!(f, "Container:{}", v.len()),
            RunOnDrop(_) => write!(f, "RunOnDrop:?"),
        }
    }
}

impl Trace for Object {
    fn trace(&self, visitor: &Visitor) {
        match self {
            Object::Simple => (),
            Object::Container(ref v) => {
                for r in v {
                    visitor.visit(r);
                }
            },
            Object::RunOnDrop(_) => (),
        }
    }
}

#[test]
fn doesnt_free_roots() {
    let mut arena = Arena::new();
    let a = arena.root(Object::Simple);
    let b = arena.root(Object::Simple);
    let col = arena.collect();
    assert_eq!(col.total, 2);
    assert_eq!(col.collected, 0);
}

#[test]
fn frees_unrooted() {
    let mut arena = Arena::new();
    let a = arena.gc(Object::Simple);
    let b = arena.gc(Object::Simple);
    let col = arena.collect();
    assert_eq!(col.total, 2);
    assert_eq!(col.collected, 2);
}

#[test]
fn visits_children() {
    let mut arena = Arena::new();
    let c = arena.gc(Object::Simple);
    let b = arena.gc(Object::Simple);
    let a = arena.root(Object::Container(vec![c]));
    let col = arena.collect();
    assert_eq!(col.total, 3);
    assert_eq!(col.collected, 1);
}

#[test]
fn calls_drop() {
    let mut arena = Arena::new();
    let mut drop_cnt = 0usize;
    
    fn _increment(i: *mut ()) {
        unsafe {
            // SAFETY: this is a pointer to drop_cnt in the scope above; it'd be safer if this were
            // a reference, and it only isn't because of laziness.
            *(i as *mut usize) += 1;
        }
    }

    let a = arena.gc(Object::RunOnDrop(
            RunOnDrop {
                func: _increment,
                data: &mut drop_cnt as *mut _ as *mut (),
            }
    ));
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 1);
    assert_eq!(drop_cnt, 1);
}

#[test]
fn rooted_borrow_lives() {
    let mut arena = Arena::new();
    let mut a = arena.root(Object::Simple);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 0);
    assert_eq!(&*a, &Object::Simple);
    assert_eq!(&mut *a, &mut Object::Simple);
}

#[test]
fn unrooted_borrow_dies() {
    let mut arena = Arena::new();
    let mut a = arena.gc(Object::Simple);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 1);
    assert_eq!(Gc::try_as_ref(&a), None);
    assert_eq!(Gc::try_as_mut(&mut a), None);
}

#[test]
fn borrow_dies_after_unroot() {
    let mut arena = Arena::new();
    let mut a = arena.root(Object::Simple);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 0);
    assert_eq!(&*a, &Object::Simple);
    assert_eq!(&mut *a, &mut Object::Simple);
    arena.unroot(&a);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 1);
    assert_eq!(Gc::try_as_ref(&a), None);
    assert_eq!(Gc::try_as_mut(&mut a), None);
}

#[test]
fn iterative() {
    let mut arena = Arena::new();
    for i in 0..5 {
        let mut a = arena.root(Object::Simple);
        let col = arena.collect();
        assert_eq!(col.total, 1);
        assert_eq!(col.collected, 0);
        assert_eq!(&*a, &Object::Simple);
        assert_eq!(&mut *a, &mut Object::Simple);
        arena.unroot(&a);
        let col = arena.collect();
        assert_eq!(col.total, 1);
        assert_eq!(col.collected, 1);
        assert_eq!(Gc::try_as_ref(&a), None);
        assert_eq!(Gc::try_as_mut(&mut a), None);
    }
}

#[test]
fn no_boxes_in_empty_arena() {
    let mut arena = Arena::new();
    assert_eq!(arena.iter().next(), None);
}

#[test]
fn no_boxes_in_reclaimed_arena() {
    let mut arena = Arena::new();
    for count in 1..5 {
        let refs = std::iter::repeat_with(|| {
            arena.gc(Object::Simple)
        }).take(count).collect::<Vec<_>>();
        let col = arena.collect();
        assert_eq!(col.total, count);
        assert_eq!(col.collected, count);
        assert_eq!(arena.iter().next(), None);
    }
}

#[test]
fn roots_outlive_refs() {
    let mut arena = Arena::new();
    let mut total = 0usize;
    for count in 1..5 {
        let refs = std::iter::repeat_with(|| {
            arena.root(Object::Simple)
        }).take(count).collect::<Vec<_>>();
        total += count;
        let col = arena.collect();
        assert_eq!(col.total, total);
        assert_eq!(col.collected, 0);
        assert!(arena.iter().next().is_some());
        assert_eq!(arena.iter().count(), total);
    }
}

// More of a compilation test than anything, but...
#[test]
fn ergonomic_matching() {
    let mut arena = Arena::new();
    let a = arena.root(Object::Simple);
    let b = arena.root(Object::Container(Vec::new()));
    let c = |x: Gc<Object>| {
        match &*x {
            Object::Simple => println!("simple object"),
            Object::Container(_) => println!("container"),
            _ => println!("something else"),
        }
    };
    c(a);
    c(b);
}

macro_rules! panic_template {
    ($test:ident, $object:ident, $code:block) => {
        #[test]
        #[should_panic(expected = "on collected object")]
        fn $test() {
            let mut arena = Arena::new();
            #[allow(unused_mut)]
            let mut $object = arena.gc(Object::Simple);  // Oops! Forgot to root it!
            arena.collect();  // Bad things here...
            $code;
        }
    };
}

panic_template!(
    panic_on_collected_deref,
    a,
    { println!("{:?}", &*a); }
);
panic_template!(
    panic_on_collected_deref_mut,
    a,
    { println!("{:?}", &mut *a); }
);
panic_template!(
    panic_on_collected_as_ref,
    a,
    { println!("{:?}", Gc::as_ref(&a)); }
);
panic_template!(
    panic_on_collected_as_mut,
    a,
    { println!("{:?}", Gc::as_mut(&mut a)); }
);

#[test]
fn later_make_root_saves() {
    let mut arena = Arena::new();
    let a = arena.gc(Object::Simple);
    // later...
    arena.make_root(&a);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 0);
    assert_eq!(&*a, &Object::Simple);
}

#[test]
fn unroot_undoes_make_root() {
    let mut arena = Arena::new();
    let a = arena.gc(Object::Simple);
    arena.make_root(&a);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 0);
    arena.unroot(&a);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 1);
    assert_eq!(Gc::try_as_ref(&a), None);
}

#[test]
fn multiple_make_roots_are_idempotent() {
    let mut arena = Arena::new();
    let a = arena.gc(Object::Simple);
    for i in 0..5 {
        arena.make_root(&a);
    }
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 0);
    arena.unroot(&a);
    let col = arena.collect();
    assert_eq!(col.total, 1);
    assert_eq!(col.collected, 1);
}

#[test]
fn same_alloc() {
    let mut arena = Arena::new();
    let a = arena.root(Object::Simple);
    let b = a.clone();
    let c = arena.root(Object::Simple);
    assert!(Gc::ptr_eq(&a, &b));
    assert!(Gc::ptr_eq(&b, &a));
    assert!(!Gc::ptr_eq(&a, &c));
    assert!(!Gc::ptr_eq(&b, &c));
}
