use crate::memory::{
    PageSize,
    GLOBAL_PMM,
};

pub const MAG_CAPACITY: usize = 256;

pub struct Magazine {
    pages: [usize; MAG_CAPACITY], // memory addrs
    count: usize,
}

impl Magazine {
    pub const fn init() -> Self { Self { pages: [0; MAG_CAPACITY], count: 0 } }

    pub fn alloc(&mut self) -> usize {
        if self.count > 0 {
            self.count -= 1;
            let ret = self.pages[self.count];
            self.pages[self.count] = 0;
            ret
        } else {
            let mut pmm = GLOBAL_PMM.lock();
            for _ in 0..(MAG_CAPACITY / 2 - 1) {
                let block = pmm.alloc(PageSize::Size4K);
                self.pages[self.count] = block.expect("Global PMM exhausted");
                self.count += 1;
            }
            let ret = pmm.alloc(PageSize::Size4K).expect("Global PMM exhausted");
            drop(pmm);
            ret
        }
    }

    pub fn free(&mut self, addr: usize) {
        if self.count < MAG_CAPACITY {
            self.pages[self.count] = addr;
            self.count += 1;
        } else {
            let mut pmm = GLOBAL_PMM.lock();
            for _ in 0..(MAG_CAPACITY / 2) {
                self.count -= 1;
                pmm.free(self.pages[self.count], PageSize::Size4K);
            }
            self.pages[self.count] = addr;
            self.count += 1;
            drop(pmm);
        }
    }
}
