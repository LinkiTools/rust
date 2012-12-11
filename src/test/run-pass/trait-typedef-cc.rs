// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// xfail-test FIXME: #3907
// aux-build:trait_typedef_cc.rs
extern mod trait_typedef_cc;

type Foo = trait_typedef_cc::Foo;

struct S {
    name: int
}

impl S: Foo {
    fn bar() { }
}

fn main() {
    let s = S {
        name: 0
    };
    s.bar();
}