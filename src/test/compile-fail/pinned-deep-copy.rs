// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// error-pattern: copying a noncopyable value

struct r {
  i: @mut int,
}

impl r : Drop {
    fn finalize(&self) {
        *(self.i) = *(self.i) + 1;
    }
}

fn r(i: @mut int) -> r {
    r {
        i: i
    }
}

fn main() {
    let i = @mut 0;
    {
        // Can't do this copy
        let x = ~~~{y: r(i)};
        let z = copy x;
        log(debug, x);
    }
    log(error, *i);
}
