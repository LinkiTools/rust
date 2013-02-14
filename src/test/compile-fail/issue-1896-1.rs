// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

type boxedFn = { theFn: fn () -> uint };

fn createClosure (closedUint: uint) -> boxedFn {
    { theFn: fn@ () -> uint { closedUint } } //~ ERROR mismatched types
}

fn main () {
    let aFn: boxedFn = createClosure(10);

    let myInt: uint = (aFn.theFn)();

    assert myInt == 10;
}
