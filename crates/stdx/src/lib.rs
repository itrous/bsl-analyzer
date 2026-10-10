pub mod batch;
pub mod case;
pub mod fs;
pub mod heap;
pub mod par_guard;
pub mod path_exclusion;
pub mod thread;

pub use itertools::Itertools;

pub fn hash_once<Hasher: std::hash::Hasher + Default>(thing: impl std::hash::Hash) -> u64 {
    std::hash::BuildHasher::hash_one(&std::hash::BuildHasherDefault::<Hasher>::default(), thing)
}

pub trait OptionExt<T> {
    fn and_if(self, condition: bool) -> Self;
}

impl<T> OptionExt<T> for Option<T> {
    fn and_if(self, condition: bool) -> Self {
        if condition {
            self
        } else {
            None
        }
    }
}

#[macro_export]
macro_rules! format_to {
    ($buf:expr) => ();
    ($buf:expr, $lit:literal $($arg:tt)*) => {
        {
            use std::fmt::Write as _;
            let _ = write!($buf, $lit $($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_option_and_if() {
        assert_eq!(Some(42).and_if(true), Some(42));
        assert_eq!(Some(42).and_if(false), None);
        assert_eq!(None::<i32>.and_if(true), None);
    }
}
