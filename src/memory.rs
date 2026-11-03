//! Memory management: a heap for Rust's `alloc` (Vec, String, Box) and a
//! physical page ("frame") allocator for device buffers.
//!
//! The MMU stays on with the firmware's identity mapping (virtual address ==
//! physical address), so a physical address can be used directly as a pointer.

use alloc::vec::Vec;
use linked_list_allocator::LockedHeap;
use spin::Mutex;

/// Size of one page / frame in bytes.
pub const PAGE_SIZE: u64 = 4096;

// In unit tests on the host, the system allocator is used instead.
#[cfg_attr(not(test), global_allocator)]
static HEAP: LockedHeap = LockedHeap::empty();

/// Hand a block of memory (reserved from the firmware during boot) to the heap.
///
/// # Safety
/// The range must be unused, writable memory that stays valid forever, and
/// this must be called only once.
pub unsafe fn init_heap(start: *mut u8, size: usize) {
    // SAFETY: forwarded from the caller.
    unsafe { HEAP.lock().init(start, size) };
}

/// Heap usage in bytes: `(used, total)`.
pub fn heap_stats() -> (usize, usize) {
    let heap = HEAP.lock();
    (heap.used(), heap.size())
}

/// UEFI memory types we care about (UEFI spec, `EFI_MEMORY_TYPE`).
pub mod kind {
    /// The kernel image (our UEFI application's code).
    pub const LOADER_CODE: u32 = 1;
    /// Memory allocated by the kernel during boot (heap, stack).
    pub const LOADER_DATA: u32 = 2;
    /// Firmware boot-services code, unused after ExitBootServices.
    pub const BOOT_CODE: u32 = 3;
    /// Firmware boot-services data, unused after ExitBootServices.
    pub const BOOT_DATA: u32 = 4;
    /// Firmware runtime-services code (must stay untouched).
    pub const RUNTIME_CODE: u32 = 5;
    /// Firmware runtime-services data (must stay untouched).
    pub const RUNTIME_DATA: u32 = 6;
    /// Free RAM.
    pub const CONVENTIONAL: u32 = 7;
    /// ACPI tables (could be reclaimed after parsing them).
    pub const ACPI_RECLAIM: u32 = 9;
    /// ACPI firmware storage (must stay untouched).
    pub const ACPI_NVS: u32 = 10;
    /// Device registers.
    pub const MMIO: u32 = 11;
}

/// One entry of the firmware's memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// UEFI memory type, see [`kind`].
    pub kind: u32,
    /// Physical start address (page aligned).
    pub start: u64,
    /// Length in pages of [`PAGE_SIZE`] bytes.
    pub pages: u64,
}

impl Region {
    /// Length in bytes.
    pub fn bytes(&self) -> u64 {
        self.pages * PAGE_SIZE
    }

    /// First address after the region.
    pub fn end(&self) -> u64 {
        self.start + self.bytes()
    }
}

/// Hands out free pages. Simple "bump" design: pages are never freed, which is
/// fine for driver buffers that live as long as the OS.
#[derive(Debug)]
struct FrameAllocator {
    free: Vec<Region>,
    used_pages: u64,
}

impl FrameAllocator {
    const fn new() -> Self {
        FrameAllocator { free: Vec::new(), used_pages: 0 }
    }

    /// Take `pages` contiguous pages from the first region big enough.
    fn take(&mut self, pages: u64) -> Option<u64> {
        let region = self.free.iter_mut().find(|r| r.pages >= pages)?;
        let addr = region.start;
        region.start += pages * PAGE_SIZE;
        region.pages -= pages;
        self.used_pages += pages;
        Some(addr)
    }

    fn free_bytes(&self) -> u64 {
        self.free.iter().map(Region::bytes).sum()
    }
}

static FRAMES: Mutex<FrameAllocator> = Mutex::new(FrameAllocator::new());
static MAP: Mutex<Vec<Region>> = Mutex::new(Vec::new());

/// Take ownership of the free RAM listed in the final memory map.
///
/// Only CONVENTIONAL memory is used. Boot-services memory would be free too,
/// but the firmware's page tables and our boot stack live there, so we leave it.
/// Page 0 is skipped so that a null pointer is never a valid buffer.
pub fn init_frames(map: Vec<Region>) {
    let mut frames = FRAMES.lock();
    frames.free = map.iter().filter(|r| r.kind == kind::CONVENTIONAL && r.start != 0).copied().collect();
    *MAP.lock() = map;
}

/// Allocate `pages` contiguous, zeroed pages. Returns the physical address.
///
/// # Panics
/// Panics when no free region is large enough.
pub fn alloc_pages(pages: u64) -> u64 {
    let addr = FRAMES.lock().take(pages).expect("out of physical memory");
    // SAFETY: the pages come from CONVENTIONAL memory that nobody else uses
    // and are identity mapped, so the physical address is a valid pointer.
    unsafe { core::ptr::write_bytes(addr as *mut u8, 0, (pages * PAGE_SIZE) as usize) };
    addr
}

/// Allocate one zeroed page. Returns the physical address.
pub fn alloc_page() -> u64 {
    alloc_pages(1)
}

/// Memory usage summary in bytes, see [`stats`].
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// All RAM the firmware reported (excluding MMIO and reserved ranges).
    pub total: u64,
    /// Free pages left for [`alloc_pages`].
    pub free: u64,
    /// Pages handed out by [`alloc_pages`].
    pub used_by_drivers: u64,
    /// Firmware runtime services and ACPI.
    pub firmware: u64,
    /// Kernel image, heap and stack.
    pub kernel: u64,
    /// Boot-services memory that could be reclaimed.
    pub reclaimable: u64,
}

/// Memory usage in bytes, summed from the memory map.
pub fn stats() -> Stats {
    use kind::*;
    let map = MAP.lock();
    let frames = FRAMES.lock();
    let sum =
        |kinds: &[u32]| -> u64 { map.iter().filter(|r| kinds.contains(&r.kind)).map(Region::bytes).sum() };
    Stats {
        total: sum(&[
            LOADER_CODE,
            LOADER_DATA,
            BOOT_CODE,
            BOOT_DATA,
            RUNTIME_CODE,
            RUNTIME_DATA,
            CONVENTIONAL,
            ACPI_RECLAIM,
            ACPI_NVS,
        ]),
        free: frames.free_bytes(),
        used_by_drivers: frames.used_pages * PAGE_SIZE,
        firmware: sum(&[RUNTIME_CODE, RUNTIME_DATA, ACPI_RECLAIM, ACPI_NVS]),
        kernel: sum(&[LOADER_CODE, LOADER_DATA]),
        reclaimable: sum(&[BOOT_CODE, BOOT_DATA]),
    }
}

/// A copy of the firmware's memory map.
pub fn regions() -> Vec<Region> {
    MAP.lock().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn frame_allocator_bumps_and_skips_small_regions() {
        let mut f = FrameAllocator::new();
        f.free = vec![
            Region { kind: kind::CONVENTIONAL, start: 0x1000, pages: 1 },
            Region { kind: kind::CONVENTIONAL, start: 0x10_0000, pages: 4 },
        ];
        assert_eq!(f.take(1), Some(0x1000));
        assert_eq!(f.take(2), Some(0x10_0000));
        assert_eq!(f.take(2), Some(0x10_2000));
        assert_eq!(f.take(1), None);
        assert_eq!(f.used_pages, 5);
        assert_eq!(f.free_bytes(), 0);
    }

    #[test]
    fn region_helpers() {
        let r = Region { kind: kind::CONVENTIONAL, start: 0x4000, pages: 2 };
        assert_eq!(r.bytes(), 0x2000);
        assert_eq!(r.end(), 0x6000);
    }
}
