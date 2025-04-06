//! Implementations of IsStoreFunction for async functions.
use super::*;

// Async 0
impl<O: 'static, Fut: Future<Output = O> + 'static, F: FnOnce() -> Fut + 'static> IsStoreFunction<()>
    for FnPair<(), F, Async>
{
    type Output = O;

    fn construct_fn(
        self,
        _input: (),
    ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Self::Output>>>> {
        Box::new(move || {
            let fut: Fut = (self.f)();
            Box::pin(fut)
        })
    }
}

macro_rules! async_impl {
    ($($i:ident),*) => {
        #[allow(non_snake_case)]
        impl<
            $($i: 'static),*,
            Fut: Future<Output = O> + 'static,
            O: 'static,
            Func: FnOnce($($i),*) -> Fut + 'static,
        > IsStoreFunction<($($i,)*)> for FnPair<($($i,)*), Func, Async> {
           type Output = O;

           fn construct_fn(
               self,
               ($($i,)*): ($($i,)*),
           ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Self::Output>>>> {
              Box::new(move || {
                  let fut: Fut = (self.f)($($i),*);
                  Box::pin(fut)
              })
           }
        }
    };
}

async_impl!(A);
async_impl!(A, B);
async_impl!(A, B, C);
async_impl!(A, B, C, D);
async_impl!(A, B, C, D, E);
async_impl!(A, B, C, D, E, F);
async_impl!(A, B, C, D, E, F, G);
async_impl!(A, B, C, D, E, F, G, H);
async_impl!(A, B, C, D, E, F, G, H, I);
async_impl!(A, B, C, D, E, F, G, H, I, J);
async_impl!(A, B, C, D, E, F, G, H, I, J, K);
async_impl!(A, B, C, D, E, F, G, H, I, J, K, L);
