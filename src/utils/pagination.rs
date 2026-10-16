//! Pagination helpers.

/// 0-based offset of a 1-based page. Page 0 is treated as page 1.
///
/// ```
/// use surrealdb_plugin::utils::pagination::offset_for;
/// assert_eq!(offset_for(1, 100), 0);
/// assert_eq!(offset_for(3, 25), 50);
/// assert_eq!(offset_for(0, 10), 0);
/// ```
pub fn offset_for(page: u64, page_size: u64) -> u64 {
    page.max(1).saturating_sub(1).saturating_mul(page_size)
}

/// Rows `[offset, offset + page_size)` of an already fetched result, plus
/// whether more rows follow. Used when the server-side `LIMIT … START …`
/// push-down is not possible.
pub fn slice_page<T>(mut rows: Vec<T>, offset: u64, page_size: u64) -> (Vec<T>, bool) {
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let size = usize::try_from(page_size).unwrap_or(usize::MAX);
    if offset >= rows.len() {
        return (Vec::new(), false);
    }
    let mut page: Vec<T> = rows.drain(offset..).collect();
    let has_more = page.len() > size;
    page.truncate(size);
    (page, has_more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        assert_eq!(offset_for(1, 100), 0);
        assert_eq!(offset_for(2, 50), 50);
        assert_eq!(offset_for(0, 10), 0);
        assert_eq!(offset_for(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn slices_pages() {
        let rows: Vec<u32> = (0..10).collect();
        assert_eq!(slice_page(rows.clone(), 0, 4), (vec![0, 1, 2, 3], true));
        assert_eq!(slice_page(rows.clone(), 8, 4), (vec![8, 9], false));
        assert_eq!(slice_page(rows.clone(), 6, 4), (vec![6, 7, 8, 9], false));
        assert_eq!(slice_page(rows, 20, 4), (Vec::<u32>::new(), false));
    }
}
