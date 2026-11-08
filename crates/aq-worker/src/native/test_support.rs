//! Test data only, independent of process-local permission constructors.
pub(crate) mod profile_history;
/// Runs `f` on a tokio worker with the default 2 MiB stack (R4), from outside any runtime.
pub(crate) fn on_worker_stack<T: Send + 'static>(f: impl Future<Output = T> + Send + 'static) -> T {
    let run = move || {
        let mut rt = tokio::runtime::Builder::new_multi_thread();
        let rt = rt.worker_threads(1).thread_stack_size(2 << 20);
        let rt = rt.enable_all().build().unwrap();
        rt.block_on(rt.spawn(f)).unwrap()
    };
    std::thread::spawn(run).join().unwrap()
}
