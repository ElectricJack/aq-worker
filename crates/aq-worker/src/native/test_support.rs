//! Test data only, independent of process-local permission constructors.
pub(crate) mod profile_history;
/// Runs `f` on a tokio worker with half tokio's 2 MiB default stack (R4), outside any runtime.
pub(crate) fn on_worker_stack<T: Send + 'static>(f: impl Future<Output = T> + Send + 'static) -> T {
    let run = move || {
        let mut rt = tokio::runtime::Builder::new_multi_thread();
        let rt = rt.worker_threads(1).thread_stack_size(1 << 20);
        let rt = rt.enable_all().build().unwrap();
        rt.block_on(rt.spawn(f)).unwrap()
    };
    std::thread::spawn(run).join().unwrap()
}
