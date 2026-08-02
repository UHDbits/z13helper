use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Default)]
pub struct SyncGuard {
    depth: Rc<Cell<u32>>,
}

impl SyncGuard {
    pub fn active(&self) -> bool {
        self.depth.get() != 0
    }

    pub fn run<T>(&self, operation: impl FnOnce() -> T) -> T {
        self.depth.set(self.depth.get().saturating_add(1));
        let _reset = Reset(self.clone());
        operation()
    }
}

struct Reset(SyncGuard);

impl Drop for Reset {
    fn drop(&mut self) {
        self.0.depth.set(self.0.depth.get().saturating_sub(1));
    }
}
