//! Identifiers.
//!
//! Deliberately small integers rather than UUIDs: a `NodeId` has to fit in an
//! ESP-NOW frame and be cheap to compare on a microcontroller.

use core::fmt;

use serde::{Deserialize, Serialize};

macro_rules! int_id {
    ($name:ident, $inner:ty, $prefix:literal) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl From<$inner> for $name {
            fn from(v: $inner) -> Self {
                Self(v)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "-{:02}"), self.0)
            }
        }
    };
}

int_id!(NodeId, u16, "node");
int_id!(JobId, u64, "job");
int_id!(TaskId, u64, "task");
