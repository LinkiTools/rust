//! Operations on managed box types

// NB: transitionary, de-mode-ing.
#[forbid(deprecated_mode)];
#[forbid(deprecated_pattern)];

use cmp::{Eq, Ord};
use intrinsic::TyDesc;

pub mod raw {
    pub struct BoxHeaderRepr {
        ref_count: uint,
        type_desc: *TyDesc,
        prev: *BoxRepr,
        next: *BoxRepr,
    }

    pub struct BoxRepr {
        header: BoxHeaderRepr,
        data: u8
    }

}

pub pure fn ptr_eq<T>(a: @T, b: @T) -> bool {
    //! Determine if two shared boxes point to the same object
    unsafe { ptr::addr_of(&(*a)) == ptr::addr_of(&(*b)) }
}

impl<T:Eq> @const T : Eq {
    #[cfg(stage0)]
    pure fn eq(other: &@const T) -> bool { *self == *(*other) }
    #[cfg(stage1)]
    #[cfg(stage2)]
    pure fn eq(&self, other: &@const T) -> bool { *(*self) == *(*other) }
    #[cfg(stage0)]
    pure fn ne(other: &@const T) -> bool { *self != *(*other) }
    #[cfg(stage1)]
    #[cfg(stage2)]
    pure fn ne(&self, other: &@const T) -> bool { *(*self) != *(*other) }
}

impl<T:Ord> @const T : Ord {
    #[cfg(stage0)]
    pure fn lt(other: &@const T) -> bool { *self < *(*other) }
    #[cfg(stage1)]
    #[cfg(stage2)]
    pure fn lt(&self, other: &@const T) -> bool { *(*self) < *(*other) }
    #[cfg(stage0)]
    pure fn le(other: &@const T) -> bool { *self <= *(*other) }
    #[cfg(stage1)]
    #[cfg(stage2)]
    pure fn le(&self, other: &@const T) -> bool { *(*self) <= *(*other) }
    #[cfg(stage0)]
    pure fn ge(other: &@const T) -> bool { *self >= *(*other) }
    #[cfg(stage1)]
    #[cfg(stage2)]
    pure fn ge(&self, other: &@const T) -> bool { *(*self) >= *(*other) }
    #[cfg(stage0)]
    pure fn gt(other: &@const T) -> bool { *self > *(*other) }
    #[cfg(stage1)]
    #[cfg(stage2)]
    pure fn gt(&self, other: &@const T) -> bool { *(*self) > *(*other) }
}

#[test]
fn test() {
    let x = @3;
    let y = @3;
    assert (ptr_eq::<int>(x, x));
    assert (ptr_eq::<int>(y, y));
    assert (!ptr_eq::<int>(x, y));
    assert (!ptr_eq::<int>(y, x));
}
