//! waiter - an intrusive doubly linked list of Future wakers
//!
//! When more than one Future may contend for a resource once it becomes available, build the list
//! with no additional memory allocations and when the resouce might be available, for at least one
//! of the waiters, awake them all, draining the list. The list is managed in FIFO order. New
//! Futures are added to the back and when they are all awoken, they are awoken from front to back.
//!
//! This module is not sound! Failure to call remove_waiter at the right time or calling it for the
//! wrong list is UB. Most modules are sound. Improper use of a sound module should leave the
//! process sound, but the desired outcome may not be reached - they are relatively easy to debug.
//! Improper use of this **unsound** module will lead to dangling pointer traversal. Such a process
//! can be very hard to debug.
//!
//! Again, this module should be considered **unsound**. It cannot prevent UB if it is used
//! incorrectly. (In the strictest sense, because even using it requires the designer wrap the
//! Elem::new() call in an `unsafe` block, one could argue it is sound. But that doesn't release
//! the designer from understanding the pitfalls.)
//!
//! Philosophy of this (and Tokio's) intrusive linked list
//! (or why no sentinel)
//!
//! Some doubly linked lists use a sentinel element to represent an empty linked list. Using a
//! sentinel avoids special handling for next and prev null pointers because while in the list, there
//! are no null pointers.
//!
//! Generally, any element in a linked list may not be moved in memory because the pointers from
//! the next and previous elements would not have been updated leading to UB. One could provide a
//! function to replace one element with another, but that would be implemented as a function that
//! reworked a few prev and next pointers, so would not be a trivial "move" of the link from one
//! location to another.
//!
//! This design requires the elements be !Unpin; they may not implement Unpin; their location may
//! not be moved. If the compiler could enforce it, we would only require them to be !Unpin while
//! they were in the linked list. But the compiler can't do that, so the elements are defined to be
//! !Unpin for their lifetime. The !Unpin is enforced by building the element with a PhantomPinned
//! marker field.
//!
//! But ... we don't want the holder of the list to have to be !Unpin, and we want to build the
//! list head directly into the holder's memory so we cannot use the list head to represent a
//! sentinel, so no sentinel in this design (nor in the Tokio internal linked list design).
//!
//! More can be read in the linked_list.rs file itself. Reading the Tokio source where the linked
//! list is used and the issues they have worked involving it over the years is a good way of
//! giving oneself a master class.

use crate::util::linked_list;
use crate::util::unsafe_cell::UnsafeCell;

use std::marker::PhantomPinned;
use std::ptr::NonNull;
use std::task::{Context, Waker};

// Logic has been extracted from broadcast.rs to provide the list and element types, List and Elem.

pub struct List {
    waiters: linked_list::LinkedList<Waiter, <Waiter as linked_list::Link>::Target>,
}

impl List {
    pub fn new() -> List {
        List {
            waiters: linked_list::LinkedList::new(),
        }
    }
}

impl Default for List {
    fn default() -> Self {
        Self::new()
    }
}

impl List {
    pub fn enqueue_waiter(&mut self, elem: &Elem, cx: &mut Context<'_>) {
        let waker = cx.waker();
        // Safety: the mutable reference is held for the duration of the list traversal and list
        // and element changes.
        unsafe {
            // Store the waker unless it is the same as already stored.
            // Queue if not already queued.
            elem.waiter.with_mut(|ptr| {
                match (*ptr).waker {
                    Some(ref w) if w.will_wake(waker) => {}
                    _ => {
                        (*ptr).waker = Some(waker.clone());
                    }
                }

                if !(*ptr).queued {
                    (*ptr).queued = true;
                    self.waiters.push_front(NonNull::new_unchecked(&mut *ptr));
                }
            });
        }
    }

    /// Removes the `elem` from self, the list. This *must* be called by the Future's drop.
    ///
    /// Failure to call this from the Future's drop can lead to immediate UB.
    /// Failure to do this will lead to a dangling pointer traversal if the list were to be
    /// accessed again.
    ///
    /// The `Elem` type is not a type that can remove itself from a list.
    ///
    /// There are two UB complications with this module, both involve this `remove_waiter` call.
    /// One is failure to call this function when the Future is dropped.
    /// The other is calling this function with the wrong list.
    ///
    /// The `elem` is not able to remove itself from a doubly linked list with its own drop method
    /// because its next and prev pointers don't point it to the List header when it happens to be
    /// at the head or tail of the list.
    ///
    /// The Elem drop does assert it is not enqueued but it could only extract itself from the list
    /// if it were lucky enough to be between the head and tail so it could use its prev and next
    /// pointers to extract itself; but even that wouldn't be safe because there could be a race
    /// with another task working the list in parallel as the Elem drop would have not a mutable
    /// reference to the list.
    ///
    /// Note! This is the biggest footgun in this module.
    ///
    /// # Safety
    ///
    /// The caller is responsible for ensuring `elem` belongs with this list. If it is enqueued,
    /// and it appears as the first or last element of the doubly linked list, but the wrong list
    /// head is provided, UB will result. Also if another element is being mutated that may be in
    /// the same list, you have UB.
    ///
    /// The original design, in the Tokio broadcast type, used a mutex to ensure only one element
    /// at a time was being mutated.
    ///
    /// # Safety2 The first safety outlines that this must be called for the List it may be
    /// enqueued on. This safety note is a reminder, as the initial comments above stated:
    ///
    ///   ** This *must* be called when the Future it is embedded in is dropped. **
    pub unsafe fn remove_waiter(&mut self, elem: &Elem) {
        // Note: There is no lock, but does hold &mut. So the caller was required to ensure sole
        // access to the list at this time. I believe holding the mutable reference serves the same
        // purpose.
        //
        // Original code:
        //     Acquire the tail lock. This is required for safety before accessing
        //     the waiter node.
        //     let mut tail = self.receiver.shared.tail.lock().unwrap();

        // Safety: the mutable reference is held for the duration of the list traversal and list
        // and element changes.
        let queued = elem.waiter.with(|ptr| unsafe { (*ptr).queued });

        if queued {
            // Remove the element
            //
            // Safety: the element may only be in this list, the caller is responsible for that.
            unsafe {
                elem.waiter.with_mut(|ptr| {
                    self.waiters.remove((&mut *ptr).into());
                    (*ptr).queued = false;
                });
            }
        }
    }

    pub fn awake_waiters(&mut self) {
        while let Some(mut waiter) = self.waiters.pop_back() {
            // Safety: the mutable reference is held for the duration of the list traversal and list
            // and element changes.
            let waiter = unsafe { waiter.as_mut() };

            assert!(waiter.queued);
            waiter.queued = false;

            let waker = waiter.waker.take().unwrap();
            waker.wake();
        }
    }

    pub fn is_empty(&self) -> bool {
        // Safety: the reference is held for the duration of the list traversal.
        self.waiters.is_empty()
    }

    pub fn len(&self) -> usize {
        // Safety: the reference is held for the duration of the list traversal.
        self.waiters.len()
    }

    pub fn len_backwards(&self) -> usize {
        // Safety: the reference is held for the duration of the list traversal.
        self.waiters.len_backwards()
    }
}

pub struct Elem {
    waiter: UnsafeCell<Waiter>,
}

impl Elem {
    /// # Safety
    ///
    /// Constructing an Elem is only safe if the `remove_waiter` method on the list it is designed
    /// for gets called from the drop method of the type that embeds Elem.
    /// Failure to do so leads to UB.
    ///
    /// Refer to the unit test below for an example.
    pub unsafe fn new() -> Elem {
        Elem {
            waiter: UnsafeCell::new(Waiter {
                queued: false,
                waker: None,
                pointers: linked_list::Pointers::new(),
                _p: PhantomPinned,
            }),
        }
    }
}

impl Drop for Elem {
    fn drop(&mut self) {
        // For those embedding this code into their source, if you understand the risks,
        // you may want to change this assert to a debug_assert, or remove it entirely.
        //
        // This function cannot be used to remove an element from a list, but it can trigger a
        // panic if it detects it was left in a list.
        let queued = self.waiter.with(|ptr| unsafe { (*ptr).queued });
        assert!(!queued);
    }
}

// Waiter has been copied from broadcast.rs.

/// An entry in the wait queue.
struct Waiter {
    /// True if queued.
    queued: bool,

    /// Future waiting to be awoken (with awake_waiters).
    waker: Option<Waker>,

    /// Intrusive linked-list pointers.
    pointers: linked_list::Pointers<Waiter>,

    /// Should not be `Unpin`.
    _p: PhantomPinned,
}

generate_addr_of_methods! {
    impl<> Waiter {
        unsafe fn addr_of_pointers(self: NonNull<Self>) -> NonNull<linked_list::Pointers<Waiter>> {
            &self.pointers
        }
    }
}

/// # Safety
///
/// `Waiter` is required and forced to be !Unpin.
unsafe impl linked_list::Link for Waiter {
    type Handle = NonNull<Waiter>;
    type Target = Waiter;

    fn as_raw(handle: &NonNull<Waiter>) -> NonNull<Waiter> {
        *handle
    }

    unsafe fn from_raw(ptr: NonNull<Waiter>) -> NonNull<Waiter> {
        ptr
    }

    unsafe fn pointers(target: NonNull<Waiter>) -> NonNull<linked_list::Pointers<Waiter>> {
        Waiter::addr_of_pointers(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::pin::Pin;
    use core::task::Poll;
    use core::cell::RefCell;
    use core::future::Future;
    use std::rc::Rc;
    use tokio::task;

    #[derive(Clone, Default)]
    /// Foo is the manager of the Futures.
    struct Foo {
        list: Rc<RefCell<List>>,
    }
    /// Bar is the Future. It may not outlive its manager.
    struct Bar<'a> {
        foo: &'a Foo,
        countdown: usize,
        elem: Elem,
    }
    // Define Bar to be Unpin, despite the Foo reference. The Foo reference is not a self
    // reference. The Future in this case must be Unpin to allow its countdown field to be
    // modified when it is polled.
    impl<'a> Unpin for Bar<'a> {}

    impl Foo {
        fn new() -> Foo {
            Foo {
                list: Default::default(),
            }
        }
        /// Returns a Future that will become ready once its polled countdown is reached.
        fn bar<'a>(&'a self, countdown: usize) -> Bar<'a> {
            Bar {
                foo: self,
                countdown,
                // Safety: This Elem is removed by Bar's drop: list.remove_waiter(&self.elem).
                elem: unsafe { Elem::new() },
            }
        }
    }

    impl<'a> Future for Bar<'a> {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.countdown == 0 {
                return Poll::Ready(());
            }
            self.countdown -= 1;
            self.foo.list.borrow_mut().enqueue_waiter(&self.elem, cx);
            Poll::Pending
        }
    }

    impl<'a> Drop for Bar<'a> {
        fn drop(&mut self) {
            let mut list = self.foo.list.borrow_mut();
            // Safety: the one unsafe call required by this `waiter` module model.
            // It is unsafe because we may not forget to call it in this Future's drop
            // and because when we do call it, the correct List manager must be used.
            unsafe {
                list.remove_waiter(&self.elem);
            }
        }
    }

    #[tokio::test]
    async fn await_waiters_synchronize() {
        // Test that the waiters linked list grows to length three when three
        // tasks should be awaiting.
        // Test that calling awake_waiters then drains the list and test that
        // the three tasks made the expected progress after getting through their
        // await points.
        let local = task::LocalSet::new();
        // Run the local task set.
        local.run_until(async move {
            // The progress string will prove how far the various tasks have gotten
            // between sync points.
            let progress = Rc::new(RefCell::new(String::from("s")));
            let foo = Foo::new();
            for _ in 0..3 {
                let foo = foo.clone();
                let progress = progress.clone();
                task::spawn_local(async move {
                    *progress.borrow_mut() += "1";
                    foo.bar(1).await; // Here we test, by awaiting the Future. awake_waiters
                                      // will have to be called once below (because countdown
                                      // is set to 1 here).
                    *progress.borrow_mut() += "2";
                });
            }
            while *progress.borrow() != "s111" {
                task::yield_now().await;
            }
            assert_eq!(foo.list.borrow_mut().len(), 3);
            foo.list.borrow_mut().awake_waiters();
            assert_eq!(foo.list.borrow_mut().len(), 0);
            while *progress.borrow() != "s111222" {
                task::yield_now().await;
            }
        }).await;
    }
}
