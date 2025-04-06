//! Implementations of IsStoreFunction for sync functions.

use super::*;

// Sync 0
impl<O: 'static, F: FnOnce() -> O + 'static> IsStoreFunction<()> for FnPair<(), F, Sync> {
    type Output = O;

    fn construct_fn(
        self,
        _input: (),
    ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Self::Output>>>> {
        Box::new(move || {
            let o: O = (self.f)();
            Box::pin(std::future::ready(o))
        })
    }
}

macro_rules! sync_impl {
    ($($i:ident),*) => {
        #[allow(non_snake_case)]
        impl<
            $($i: 'static),*,
            O: 'static,
            Func: FnOnce($($i),*) -> O + 'static,
        > IsStoreFunction<($($i,)*)> for FnPair<($($i,)*), Func, Sync> {
           type Output = O;

           fn construct_fn(
               self,
               ($($i,)*): ($($i,)*),
           ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Self::Output>>>> {
              Box::new(move || {
                  let o: O = (self.f)($($i),*);
                  Box::pin(std::future::ready(o))
              })
           }
        }
    };
}

sync_impl!(A);
sync_impl!(A, B);
sync_impl!(A, B, C);
sync_impl!(A, B, C, D);
sync_impl!(A, B, C, D, E);
sync_impl!(A, B, C, D, E, F);
sync_impl!(A, B, C, D, E, F, G);
sync_impl!(A, B, C, D, E, F, G, H);
sync_impl!(A, B, C, D, E, F, G, H, I);
sync_impl!(A, B, C, D, E, F, G, H, I, J);
sync_impl!(A, B, C, D, E, F, G, H, I, J, K);
sync_impl!(A, B, C, D, E, F, G, H, I, J, K, L);
