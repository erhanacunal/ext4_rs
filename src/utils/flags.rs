//! A small stand-in for the `bitflags` crate: the part of its API this crate uses.

/// Declares a newtype over an integer with named constants, `bits`, `from_bits` (`None` when an
/// unknown bit is set), `from_bits_truncate`, `contains` and `|`.
#[macro_export]
macro_rules! flags {
    (
        $(#[$meta:meta])*
        pub struct $name:ident: $ty:ty {
            $(const $flag:ident = $value:expr;)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name($ty);

        #[allow(non_upper_case_globals)]
        impl $name {
            $(pub const $flag: Self = Self($value);)*

            /// Every named bit.
            const ALL: $ty = 0 $(| $value)*;

            pub const fn bits(&self) -> $ty {
                self.0
            }

            pub const fn from_bits(bits: $ty) -> Option<Self> {
                if bits & !Self::ALL == 0 { Some(Self(bits)) } else { None }
            }

            pub const fn from_bits_truncate(bits: $ty) -> Self {
                Self(bits & Self::ALL)
            }

            pub const fn contains(&self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }
        }

        impl core::ops::BitOr for $name {
            type Output = Self;

            fn bitor(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }
        }
    };
}
