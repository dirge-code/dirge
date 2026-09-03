#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]
#![no_std]

extern crate alloc;

use core::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

impl fmt::Debug for Janet {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(core::any::type_name::<Self>())
    }
}

impl PartialEq<Janet> for Janet {
    #[inline]
    fn eq(&self, other: &Janet) -> bool {
        unsafe { janet_equals(*self, *other) != 0 }
    }
}

impl Eq for Janet {}

impl Hash for Janet {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_i32(unsafe { janet_hash(*self) })
    }
}

impl PartialOrd<Janet> for Janet {
    #[inline]
    fn partial_cmp(&self, other: &Janet) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Janet {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        let res = unsafe { janet_compare(*self, *other) };

        match res {
            -1 => Ordering::Less,
            0 => Ordering::Equal,
            1 => Ordering::Greater,
            // SAFETY: Janet ensures that the only values that `janet_compare` will return is `0`,
            // `1` and `-1`.
            _ => unsafe { core::hint::unreachable_unchecked() },
        }
    }
}
