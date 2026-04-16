//! Utilities for tearing down and freeing page table hierarchies.

use super::pg_tables::{PDPTable, PML4Table, PTable};
use crate::error::Result;
use crate::memory::paging::TableMapper;
use crate::memory::{
    address::{PA, TPA},
    paging::{
        PaMapper, PageTableMapper, PgTable, PgTableArray, tear_down::RecursiveTeardownWalker,
        walk::WalkContext,
    },
};

// Implementation for PTable (Leaf Table)
impl RecursiveTeardownWalker for PTable {
    fn tear_down<F, PM>(
        table_pa: TPA<PgTableArray<Self>>,
        ctx: &mut WalkContext<PM>,
        deallocator: &mut F,
    ) -> Result<()>
    where
        PM: PageTableMapper,
        F: FnMut(PA),
    {
        unsafe {
            ctx.mapper.with_page_table(table_pa, |pgtable| {
                let table = PTable::from_ptr(pgtable);

                for idx in 0..Self::DESCRIPTORS_PER_PAGE {
                    let desc = table.get_idx(idx);

                    if let Some(addr) = desc.mapped_address() {
                        deallocator(addr);
                    }
                }
            })?;
        }

        Ok(())
    }
}

/// Walks the page table hierarchy for a given address space and applies a
/// freeing closure to every lower-half canonical address allocated frame.
///
/// # Parameters
/// - `pml4_table`: The physical address of the root (PML4) page table.
/// - `ctx`: The context for the operation (mapper).
/// - `deallocator`: A closure called for every physical address that needs freeing.
///   This includes:
///     1. The User Data frames (Payload).
///     2. The PDP, PD, and PTable Page Table frames.
///     3. The PML4 Root Table frame.
///     
///  *Note* PDPTables which are pointed to by PML4 indexes [256-511] are not
///  free'd.
pub fn tear_down_address_space<F, PM>(
    pml4_table: TPA<PgTableArray<PML4Table>>,
    ctx: &mut WalkContext<PM>,
    mut deallocator: F,
) -> Result<()>
where
    PM: PageTableMapper,
    F: FnMut(PA),
{
    let mut cursor = 0;

    loop {
        let next_item = unsafe {
            ctx.mapper.with_page_table(pml4_table, |pml4_tbl| {
                let table = PML4Table::from_ptr(pml4_tbl);
                for i in cursor..256 {
                    if let Some(addr) = table.get_idx(i).next_table_address() {
                        return Some((i, addr));
                    }
                }
                None
            })?
        };

        match next_item {
            Some((idx, pdp_addr)) => {
                PDPTable::tear_down(pdp_addr, ctx, &mut deallocator)?;
                deallocator(pdp_addr.to_untyped());
                cursor = idx + 1;
            }
            None => break,
        }
    }

    deallocator(pml4_table.to_untyped());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::x86_64::memory::pg_tables::tests::TestHarness;
    use crate::memory::address::VA;
    use crate::memory::paging::permissions::PtePermissions;
    use std::collections::HashSet;

    fn capture_freed_pages<PM: PageTableMapper>(
        root_table: TPA<PgTableArray<PML4Table>>,
        ctx: &mut WalkContext<PM>,
    ) -> HashSet<usize> {
        let mut freed_set = HashSet::new();
        tear_down_address_space(root_table, ctx, |pa| {
            if !freed_set.insert(pa.value()) {
                panic!(
                    "Double free detected! Physical Address {:?} was freed twice.",
                    pa
                );
            }
        })
        .expect("Teardown failed");
        freed_set
    }

    #[test]
    fn teardown_empty_table() {
        let mut harness = TestHarness::new(5);

        let freed = capture_freed_pages(
            harness.inner.root_table,
            &mut harness.inner.create_walk_ctx(),
        );

        // Only the Root L0 table itself is freed.
        assert_eq!(freed.len(), 1);
        assert!(freed.contains(&harness.inner.root_table.value()));
    }

    #[test]
    fn teardown_single_page_hierarchy() {
        let mut harness = TestHarness::new(10);
        let va = VA::from_value(0x1_0000_0000);
        let pa = 0x8_0000;

        // Map a single 4k page.
        harness
            .map_4k_pages(pa, va.value(), 1, PtePermissions::ro(false))
            .unwrap();

        let freed = capture_freed_pages(
            harness.inner.root_table,
            &mut harness.inner.create_walk_ctx(),
        );

        // 1 Payload Page (0x80000)
        // 1 PTable
        // 1 PD Table
        // 1 PDP Table
        // 1 PML4 Table (Root)
        assert_eq!(freed.len(), 5);
        assert!(freed.contains(&pa)); // The payload
        assert!(freed.contains(&harness.inner.root_table.value())); // The root
    }

    #[test]
    fn teardown_sparse_ptable() {
        let mut harness = TestHarness::new(10);

        // Map index 0 of an PTable
        let va1 = VA::from_value(0x1_0000_0000);
        let pa1 = 0xAAAA_0000;

        // Map index 511 of the SAME PTable
        // (Assuming 4k pages, add 511 * 4096)
        let va2 = va1.add_pages(511);
        let pa2 = 0xBBBB_0000;

        harness
            .map_4k_pages(pa1, va1.value(), 1, PtePermissions::rw(false))
            .unwrap();
        harness
            .map_4k_pages(pa2, va2.value(), 1, PtePermissions::rw(false))
            .unwrap();

        let freed = capture_freed_pages(
            harness.inner.root_table,
            &mut harness.inner.create_walk_ctx(),
        );

        // 2 Payload Pages
        // 1 PML4 Table (shared)
        // 1 PD Table
        // 1 PDP Table
        // 1 PML4 Table
        assert_eq!(freed.len(), 6);
        assert!(freed.contains(&pa1));
        assert!(freed.contains(&pa2));
    }

    #[test]
    fn teardown_discontiguous_tables() {
        let mut harness = TestHarness::new(20);

        let va1 = VA::from_value(0x1_0000_0000);
        harness
            .map_4k_pages(0xA0000, va1.value(), 1, PtePermissions::rw(false))
            .unwrap();

        let va2 = VA::from_value(0x400_0000_0000);
        harness
            .map_4k_pages(0xB0000, va2.value(), 1, PtePermissions::rw(false))
            .unwrap();

        let freed = capture_freed_pages(
            harness.inner.root_table,
            &mut harness.inner.create_walk_ctx(),
        );

        // 2 Payload Pages
        // 2 PTable (one for each branch)
        // 2 PD Tables (one for each branch)
        // 2 PDP Tables (one for each branch)
        // 1 PML4 Table (Shared root)
        assert_eq!(freed.len(), 9);
    }

    #[test]
    fn teardown_full_ptable() {
        let mut harness = TestHarness::new(10);
        let start_va = VA::from_value(0x1_0000_0000);
        let start_pa = 0x10_0000;

        // Fill an entire PTable table (512 entries)
        harness
            .map_4k_pages(start_pa, start_va.value(), 512, PtePermissions::ro(false))
            .unwrap();

        let freed = capture_freed_pages(
            harness.inner.root_table,
            &mut harness.inner.create_walk_ctx(),
        );

        // 512 Payload Pages
        // 1 PTable
        // 1 PD Table
        // 1 PDP Table
        // 1 PML4 Table
        assert_eq!(freed.len(), 512 + 4);
    }

    #[test]
    fn teardown_does_not_free_kernel_tables() {
        let mut harness = TestHarness::new(15);

        // Map a page in userspace (PML4 index 0).
        let user_pa = 0x1_0000;
        harness
            .map_4k_pages(user_pa, 0x0000_0001_0000_0000, 1, PtePermissions::rw(false))
            .unwrap();

        // Map a page at a canonical kernel VA (PML4 index 256, bits [63:48] = 0xFFFF).
        // This populates the upper half of the PML4, which tear_down_address_space
        // must NOT touch.
        let kernel_pa = 0x2_0000;
        harness
            .map_4k_pages(
                kernel_pa,
                0xFFFF_8000_0001_0000usize,
                1,
                PtePermissions::rw(false),
            )
            .unwrap();

        let freed = capture_freed_pages(
            harness.inner.root_table,
            &mut harness.inner.create_walk_ctx(),
        );

        // Only the userspace hierarchy must be freed:
        //   1 payload (user_pa)
        //   1 PT table  (userspace branch)
        //   1 PD table  (userspace branch)
        //   1 PDP table (userspace branch)
        //   1 PML4 root
        assert_eq!(freed.len(), 5);
        assert!(freed.contains(&user_pa));

        // The kernel payload and its intermediate tables (PML4 index >= 256) must
        // not be freed — they belong to the kernel and outlive this address space.
        assert!(!freed.contains(&kernel_pa));
    }
}
