enum test {
    quot_zero = 1/0, //~ERROR expected constant: attempted quotient with a divisor of zero
    rem_zero = 1%0  //~ERROR expected constant: attempted remainder with a divisor of zero
}

fn main() {}
