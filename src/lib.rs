pub mod msg;

#[macro_export]
macro_rules! test_harness {
    (|| $body:block) => {
        // don't use proptest
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
            .block_on(async move $body);
    };
    (|($($v:tt)+)| $body:block) => {
        crate::test_harness!(init {}, |_init: (), ($($v)+)| $body)
    };
    (init $init:block, |$initpat:ident $(: $initty:ty)?, ($($v:tt)+)| $body:block) => {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let init = rt.block_on(async move $init);
        proptest!(|($($v)+)| {
            let $initpat $(: $initty)? = init;
            rt.block_on(async move {
                $body;
                Ok::<(), proptest::test_runner::TestCaseError>(())
            })?;
        });
    };
}
