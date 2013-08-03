// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

fn mk_identity<T>() -> @fn(T) -> T {
    let result: @fn(t: T) -> T = |t| t;
    result
}

fn main() {
    // type of r is @fn(X) -> X
    // for some fresh X
    let r = mk_identity();

    // @mut int <: X
    r(@mut 3);

    // @int <: X
    //
    // Here the type check fails because @const is gone and there is no
    // supertype.
    r(@3);  //~ ERROR mismatched types

    // Here the type check succeeds.
    *r(@mut 3) = 4;
}
