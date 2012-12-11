// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

fn wants_static_fn(_x: &static/fn()) {}

fn main() {
    let i = 3;
    do wants_static_fn { //~ ERROR cannot infer an appropriate lifetime due to conflicting requirements
        debug!("i=%d", i);
    }
}
