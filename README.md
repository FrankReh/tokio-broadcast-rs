# tokio-broadcast-rs

## module: broadcast
A stand-alone copy of the tokio-rs sync::broadcast type. It has no dependencies. It *should* work in
any async Rust environment. It also has no footguns I am aware of as it was lifted from the Tokio
source with minimal changes.

## module: waiter
Warning: this module is *unsound*. Its misuse will lead to undefined behavior.

A new module, **waiter**, based on the linked list components of the Tokio broadcast code allowing
intrusive components. It provides two types: one embeds directly in the future handler and
one type embeds directly into the future struct.

The **waiter::List** and **waiter::Elem** types provide a mechanism for storing futures' waker
functions in an intrusive doubly linked list allowing efficient management of the list, including
removing futures that are dropped that were not allowed to complete.

This repo is meant less as a crate to add to one's dependencies and more as code that can be
copied and incorporated directly in a project. Everyone is strongly advised to stay clear of
this module unless they understand why it is considered unsound.

### Footgun warning about the unsound waiter module
The **waiter** module comes with a very big footgun. To use this module, it is important to
use the *remove_waiter* method properly. A design that does not call it at the right time is UB.

It bears repeating. This module is not sound because not calling *remove_waiter* when the future is
dropped can lead to dangling pointers. It must be called by the future's drop method.

Most rust crates do not expose something so dangerous to a program's soundness. It is presented here
because some would want to build with an efficient mechanism and are prepared to be as careful as
necessary, just as the original authors of the linked list and the use of it for waker management
chose.
