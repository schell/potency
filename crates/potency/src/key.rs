//! Turning parameters into string keys.

pub trait AsKey {
    fn as_key(&self) -> String;
}

impl AsKey for () {
    fn as_key(&self) -> String {
        "()".to_owned()
    }
}

impl AsKey for String {
    fn as_key(&self) -> String {
        self.clone()
    }
}

impl AsKey for &str {
    fn as_key(&self) -> String {
        self.to_string()
    }
}

impl AsKey for u32 {
    fn as_key(&self) -> String {
        self.to_string()
    }
}

impl AsKey for f32 {
    fn as_key(&self) -> String {
        self.to_string()
    }
}

impl<T: AsKey> AsKey for Vec<T> {
    fn as_key(&self) -> String {
        format!(
            "v[{}]",
            self.iter().map(T::as_key).collect::<Vec<_>>().join(",")
        )
    }
}

impl<T: AsKey, const N: usize> AsKey for [T; N] {
    fn as_key(&self) -> String {
        format!(
            "a{N}[{}]",
            self.iter().map(T::as_key).collect::<Vec<_>>().join(",")
        )
    }
}

impl<T: AsKey> AsKey for &[T] {
    fn as_key(&self) -> String {
        format!(
            "a[{}]",
            self.iter().map(T::as_key).collect::<Vec<_>>().join(",")
        )
    }
}

macro_rules! as_key_tuple_impl {
    ($($i:ident),*) => {
        #[allow(non_snake_case)]
        impl< $($i: AsKey),* > AsKey for ($($i),*) {
            fn as_key(&self) -> String {
                let ($($i),*) = self;
                vec![$($i.as_key()),*].join(",")
            }
        }
    };
}

as_key_tuple_impl!(A, B);
as_key_tuple_impl!(A, B, C);
as_key_tuple_impl!(A, B, C, D);
as_key_tuple_impl!(A, B, C, D, E);
as_key_tuple_impl!(A, B, C, D, E, F);
as_key_tuple_impl!(A, B, C, D, E, F, G);
as_key_tuple_impl!(A, B, C, D, E, F, G, H);
as_key_tuple_impl!(A, B, C, D, E, F, G, H, I);
as_key_tuple_impl!(A, B, C, D, E, F, G, H, I, J);
as_key_tuple_impl!(A, B, C, D, E, F, G, H, I, J, K);
as_key_tuple_impl!(A, B, C, D, E, F, G, H, I, J, K, L);
