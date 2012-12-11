// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern mod std;

fn main() {
    let v =
        vec::map2(~[1, 2, 3, 4, 5],
                  ~[true, false, false, true, true],
                  |i, b| if *b { -(*i) } else { *i } );
    log(error, copy v);
    assert (v == ~[-1, 2, 3, -4, -5]);
}
