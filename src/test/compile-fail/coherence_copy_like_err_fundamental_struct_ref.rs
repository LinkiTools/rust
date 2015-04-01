// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Test that we are able to introduce a negative constraint that
// `MyType: !MyTrait` along with other "fundamental" wrappers.

// aux-build:coherence_copy_like_lib.rs

#![feature(rustc_attrs)]
#![allow(dead_code)]

extern crate coherence_copy_like_lib as lib;

use std::marker::MarkerTrait;

struct MyType { x: i32 }

trait MyTrait : MarkerTrait { }
impl<T: lib::MyCopy> MyTrait for T { }

// `MyFundamentalStruct` is declared fundamental, so we can test that
//
//    MyFundamentalStruct<&MyTrait>: !MyTrait
//
// Huzzah.
impl<'a> MyTrait for lib::MyFundamentalStruct<&'a MyType> { }

#[rustc_error]
fn main() { } //~ ERROR compilation successful
