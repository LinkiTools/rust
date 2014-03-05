// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[feature(managed_boxes)];

fn wants_uniq(x: Vec<uint> ) { }
fn wants_three(x: [uint, ..3]) { }

fn has_uniq(x: Vec<uint> ) {
   wants_uniq(x);
   wants_three(x); //~ ERROR [] storage differs: expected `3` but found `~`
}

fn has_three(x: [uint, ..3]) {
   wants_uniq(x); //~ ERROR [] storage differs: expected `~` but found `3`
   wants_three(x);
}

fn has_four(x: [uint, ..4]) {
   wants_uniq(x); //~ ERROR [] storage differs: expected `~` but found `4`
   wants_three(x); //~ ERROR [] storage differs: expected `3` but found `4`
}

fn main() {
}
