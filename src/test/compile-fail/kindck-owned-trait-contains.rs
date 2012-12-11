// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

trait repeat<A> { fn get() -> A; }

impl<A:Copy> @A: repeat<A> {
    fn get() -> A { *self }
}

fn repeater<A:Copy>(v: @A) -> repeat<A> {
    // Note: owned kind is not necessary as A appears in the trait type
    v as repeat::<A> // No
}

fn main() {
    // Here, an error results as the type of y is inferred to
    // repeater<&lt/3> where lt is the block.
    let y = {
        let x: &blk/int = &3; //~ ERROR cannot infer an appropriate lifetime
        repeater(@x)
    };
    assert 3 == *(y.get());
}