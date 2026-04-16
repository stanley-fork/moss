use crate::memory::{
    address::{PA, TPA},
    paging::{
        PageTableMapper, PgTable, PgTableArray, TableMapper, TableMapperTable, walk::WalkContext,
    },
};

pub trait RecursiveTeardownWalker: PgTable + Sized {
    fn tear_down<F, PM>(
        table_pa: TPA<PgTableArray<Self>>,
        ctx: &mut WalkContext<PM>,
        deallocator: &mut F,
    ) -> crate::error::Result<()>
    where
        PM: PageTableMapper,
        F: FnMut(PA);
}

// Blanket impl for TableMapperTable types.
impl<T> RecursiveTeardownWalker for T
where
    T: TableMapperTable,
    <T::Descriptor as TableMapper>::NextLevel: RecursiveTeardownWalker,
{
    fn tear_down<F, PM>(
        table_pa: TPA<PgTableArray<Self>>,
        ctx: &mut WalkContext<PM>,
        deallocator: &mut F,
    ) -> crate::error::Result<()>
    where
        PM: PageTableMapper,
        F: FnMut(PA),
    {
        let mut cursor = 0;

        loop {
            let next_item = unsafe {
                ctx.mapper.with_page_table(table_pa, |pgtable| {
                    let table = Self::from_ptr(pgtable);

                    for i in cursor..<T as PgTable>::DESCRIPTORS_PER_PAGE {
                        let desc = table.get_idx(i);

                        if let Some(addr) = desc.next_table_address() {
                            return Some((i, addr));
                        }
                    }
                    None
                })?
            };

            match next_item {
                Some((found_idx, phys_addr)) => {
                    // Recurse first
                    <T::Descriptor as TableMapper>::NextLevel::tear_down(
                        phys_addr,
                        ctx,
                        deallocator,
                    )?;

                    // Free the child table frame
                    deallocator(phys_addr.to_untyped());

                    // Advance cursor to skip this entry next time
                    cursor = found_idx + 1;
                }
                None => {
                    // No more valid entries in this table.
                    break;
                }
            }
        }

        Ok(())
    }
}
