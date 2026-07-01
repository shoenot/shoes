#![allow(dead_code)]
use core::ops::{
    BitAnd,
    BitOr,
    Not,
    Shl,
};

#[inline]
pub fn set_bit<T>(value: T, bit: u8) -> T
where
    T: BitOr<T, Output = T> + Shl<u8, Output = T> + From<u8>,
{
    value | (T::from(1) << bit)
}

#[inline]
pub fn unset_bit<T>(value: T, bit: u8) -> T
where
    T: BitAnd<T, Output = T> + Shl<u8, Output = T> + From<u8> + Not<Output = T>,
{
    value & !(T::from(1) << bit)
}

#[inline]
pub fn check_bit<T>(value: T, bit: u8) -> bool
where
    T: BitAnd<T, Output = T> + Shl<u8, Output = T> + From<u8> + PartialEq<T> + Not<Output = T>,
{
    (value & (T::from(1) << bit)) != T::from(0)
}

#[macro_export]
macro_rules! define_bitflags {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident($ty:ty) {
            $(
                $(#[$flag_meta:meta])*
                $flag_name:ident = $value:expr;
            )*
        }) => { $(#[$meta])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
        $vis struct $name(pub $ty);

        impl $name {
            $(
                $(#[$flag_meta])*
                pub const $flag_name: Self = Self($value);
            )*

            #[inline(always)]
            pub const fn new() -> Self {
                Self(0)
            }

            #[inline(always)]
            pub const fn all() -> Self {
                Self(<$ty>::MAX)
            }

            #[inline(always)]
            pub const fn from(value: $ty) -> Self {
                Self(value)
            }

            #[inline(always)]
            pub const fn contains(self, other: Self) -> bool {
                (self.0 & other.0) == other.0
            }

            #[inline(always)]
            pub const fn insert(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }

            #[inline(always)]
            pub const fn remove(self, other: Self) -> Self {
                Self(self.0 & !other.0)
            }

            #[inline(always)]
            pub const fn union(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }
            
            #[inline(always)]
            pub const fn intersection(self, other: Self) -> Self {
                Self(self.0 & other.0)
            }
            
            #[inline(always)]
            pub const fn difference(self, other: Self) -> Self {
                Self(self.0 & !other.0)
            }
            
            #[inline(always)]
            pub const fn complement(self) -> Self {
                Self(!self.0)
            }
            
            #[inline(always)]
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }
            
            #[inline(always)]
            pub const fn bits(self) -> $ty {
                self.0
            }
        }

        impl core::ops::BitOr for $name {
            type Output = Self;
            #[inline(always)]
            fn bitor(self, rhs: Self) -> Self { self.union(rhs) }
        }

        impl core::ops::BitAnd for $name {
            type Output = Self;
            #[inline(always)]
            fn bitand(self, rhs: Self) -> Self { self.intersection(rhs) }
        }

        impl core::ops::Not for $name {
            type Output = Self;
            #[inline(always)]
            fn not(self) -> Self { self.complement() }
        }

        impl core::ops::Sub for $name {
            type Output = Self;
            #[inline(always)]
            fn sub(self, rhs: Self) -> Self { self.difference(rhs) }
        }
    };
}
