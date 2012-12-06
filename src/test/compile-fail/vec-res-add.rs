// error-pattern: instantiating a type parameter with an incompatible type

struct r {
  i:int
}

fn r(i:int) -> r { r { i: i } }

impl r : Drop {
    fn finalize(&self) {}
}

fn main() {
    // This can't make sense as it would copy the classes
    let i = move ~[r(0)];
    let j = move ~[r(1)];
    let k = i + j;
    log(debug, j);
}
